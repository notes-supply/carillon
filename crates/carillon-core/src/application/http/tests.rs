use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::Context as _;
use axum::{response::IntoResponse as _, routing::post};
use tempfile::tempdir;

use super::*;
use crate::db::Database;
use carillon_protocol::model::{DeliveryOutcome, TurnState};

fn installation(root: &Path) -> Result<AppState> {
    use carillon_installation::config::{AgentConfig, Config, CoreConfig, Provider, hash_token};

    // Core tests use opaque provider IDs, without a Discord or Pi installation.
    // Native session state belongs to the runtime.
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
        agent: AgentConfig {
            id: "test-agent".to_owned(),
            name: "Test agent".to_owned(),
            resource_root: resources,
            workspace: root.to_path_buf(),
            runtime_provider: "test-runtime".to_owned(),
        },
    };
    value.validate()?;
    let config = LoadedConfig {
        root: root.to_path_buf(),
        value,
    };
    Ok(AppState {
        core: Core::create(core_options(&config))?,
        config,
        client: Client::new(),
    })
}

// These tests mutate configuration in memory to exercise ownership changes.
// Recompose policy without reopening storage: fault-injection tests deliberately alter its schema.
fn configured(state: &AppState) -> AppState {
    let mut state = state.clone();
    state.core.options = core_options(&state.config);
    state
}

async fn run_next_turn(
    state: &AppState,
    runtime_id: &str,
    submit: impl AsyncFnOnce(
        &Provider,
        &str,
        RuntimeTurnRequest,
    ) -> Result<RuntimeTurnResponse, TurnFailure>,
) -> Result<WorkerStep> {
    super::run_next_turn(&configured(state), runtime_id, submit).await
}

async fn accept(state: &AppState, id: &str, audience: &str) -> Result<EventReceipt> {
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
            agent_id: state.config.value.agent.id.clone(),
            user_id: "test-user".to_owned(),
            audience_id: audience.to_owned(),
            delivery_target_id: "test-destination".to_owned(),
            feedback_target_id: "test-feedback".to_owned(),
            content: id.to_owned(),
            native: serde_json::Value::Null,
            attachments: None,
        }),
    )
    .await
    .map_err(|error| anyhow!(error.message))?;
    Ok(receipt.0)
}

fn completed(request: RuntimeTurnRequest) -> RuntimeTurnResponse {
    RuntimeTurnResponse {
        correlation_id: request.correlation_id,
        output: format!("reply to {}", request.input),
        follow_up: None,
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
            assert!(LoadedConfig::load(&path).is_err(), "accepted {url:?}");
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
            assert_eq!(
                LoadedConfig::load(&path).is_ok(),
                valid,
                "parallel_turns {parallel} on {:?}",
                state.config.value.providers[index].role
            );
        }
        state.config.value.providers[index].parallel_turns = None;
    }
    state.config.value.core.listen.set_port(0);
    fs::write(&path, toml::to_string(&state.config.value)?)?;
    assert!(
        LoadedConfig::load(&path).is_err(),
        "accepted core port zero"
    );
    Ok(())
}

#[tokio::test]
async fn status_probes_public_health_without_provider_credentials() -> Result<()> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mut services = tokio::task::JoinSet::new();
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    for provider in &mut state.config.value.providers {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        provider.url = format!("http://{}", listener.local_addr()?);
        let report = provider_status(provider, "ready");
        let sent = sent.clone();
        let router = Router::new().route(
            "/health",
            get(move |headers: HeaderMap| {
                let sent = sent.clone();
                let report = report.clone();
                async move {
                    let _ = sent.send(headers);
                    Json(report)
                }
            }),
        );
        services.spawn(async move { axum::serve(listener, router).await });
    }
    let router = Router::new()
        .route("/v1/status", get(status))
        .with_state(state);
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/v1/status").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        received.try_recv().is_err(),
        "unauthenticated status probed providers"
    );

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        router.oneshot(
            Request::builder()
                .uri("/v1/status")
                .header(http::header::AUTHORIZATION, "Bearer test-operator")
                .body(Body::empty())?,
        ),
    )
    .await??;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    let report: StatusReport = serde_json::from_slice(&body)?;
    assert_eq!(report.release, RELEASE_VERSION);
    assert_eq!(report.providers.len(), 3);
    assert!(
        report
            .providers
            .iter()
            .all(|provider| provider.status == "ready")
    );
    for _ in 0..2 {
        let headers = received.try_recv()?;
        assert!(
            !headers.contains_key(http::header::AUTHORIZATION),
            "public health leaked a credential"
        );
    }
    Ok(())
}

#[tokio::test]
async fn provider_health_mismatches_keep_configured_identity_unavailable() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let mut services = tokio::task::JoinSet::new();
    for provider in &state.config.value.providers {
        for field in ["id", "role", "version"] {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let mut configured = provider.clone();
            configured.url = format!("http://{}", listener.local_addr()?);
            let mut report = serde_json::to_value(provider_status(provider, "ready"))?;
            report[field] = json!("unexpected");
            let router = Router::new().route(
                "/health",
                get(move || {
                    let report = report.clone();
                    async move { Json(report) }
                }),
            );
            services.spawn(async move { axum::serve(listener, router).await });
            let result = fetch_health(&state.client, &configured).await;
            assert_eq!(result.id, configured.id);
            assert_eq!(result.role, configured.role.as_str());
            if field == "version" {
                assert_eq!(result.version, "unexpected");
                assert_eq!(result.status, "version_mismatch");
            } else {
                assert_eq!(result.version, configured.version);
                assert_eq!(result.status, "unavailable", "accepted mismatched {field}");
            }
        }
    }
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
    assert!(serve(state.config.clone()).await.is_err());
    assert!(!path.exists());
    Ok(())
}

/// A runtime that records every model call and rejects one selection the way
/// a real runtime phrases it for people.
fn fake_runtime(
    id: &'static str,
    sent: tokio::sync::mpsc::UnboundedSender<(&'static str, serde_json::Value)>,
) -> Router {
    fn bearer(headers: &HeaderMap) -> Option<&str> {
        headers.get(http::header::AUTHORIZATION)?.to_str().ok()
    }
    let recorded = sent.clone();
    let listed = sent.clone();
    Router::new()
        .route(
            "/v1/commands",
            get(
                move |headers: HeaderMap,
                      Query(query): Query<std::collections::HashMap<String, String>>| {
                    let sent = listed.clone();
                    async move {
                        let _ = sent.send((id, json!({"auth": bearer(&headers), "query": query})));
                        Json(json!({"runtime": id, "commands": [{"name": "compact"}]}))
                    }
                },
            ),
        )
        .route(
        "/v1/model",
        get(
            move |headers: HeaderMap,
                  Query(query): Query<std::collections::HashMap<String, String>>| {
                let sent = recorded.clone();
                async move {
                    let _ = sent.send((id, json!({"auth": bearer(&headers), "query": query})));
                    Json(json!({"runtime": id, "available": []}))
                }
            },
        )
        .put(
            move |headers: HeaderMap, Json(update): Json<serde_json::Value>| {
                let sent = sent.clone();
                async move {
                    let _ = sent.send((id, json!({"auth": bearer(&headers), "update": update})));
                    if update["selection"]["model"] == json!("missing") {
                        return (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            Json(json!({"error": "That model is not available."})),
                        )
                            .into_response();
                    }
                    Json(json!({"runtime": id, "updated": true})).into_response()
                }
            },
        ),
    )
}

/// Two fake runtimes: `test-runtime` owns the `pinned` conversation and
/// `replacement` has become the agent default.
async fn model_fixture(
    directory: &Path,
) -> Result<(
    AppState,
    tokio::sync::mpsc::UnboundedReceiver<(&'static str, serde_json::Value)>,
    tokio::task::JoinSet<std::io::Result<()>>,
)> {
    let mut state = installation(directory)?;
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
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
            directory.join(format!("credentials/{id}.token")),
            "test-only",
        )?;
        if id == "test-runtime" {
            state.config.value.providers[1] = provider;
        } else {
            state.config.value.providers.push(provider);
        }
        let app = fake_runtime(id, sent.clone());
        services.spawn(async move { axum::serve(listener, app).await });
    }
    accept(&state, "one", "pinned").await?;
    state.config.value.agent.runtime_provider = "replacement".to_owned();
    Ok((state, received, services))
}

async fn model_call(
    state: &AppState,
    method: &str,
    uri: &str,
    authorized: bool,
    body: Option<&serde_json::Value>,
) -> Result<(StatusCode, serde_json::Value)> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let router = Router::new()
        .route("/v1/model", get(model_state).put(set_model))
        .route("/v1/commands", get(commands))
        .with_state(configured(state));
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(PROVIDER_HEADER, "test-interface");
    if authorized {
        request = request.header(http::header::AUTHORIZATION, "Bearer test-only");
    }
    let body = match body {
        Some(body) => Body::from(serde_json::to_vec(body)?),
        None => Body::empty(),
    };
    let response =
        tokio::time::timeout(Duration::from_secs(10), router.oneshot(request.body(body)?))
            .await??;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

#[tokio::test]
async fn model_calls_follow_conversation_ownership() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = model_fixture(directory.path()).await?;
    let (status, _) =
        model_call(&state, "GET", "/v1/model?agent_id=test-agent", false, None).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = model_call(&state, "GET", "/v1/model?agent_id=other", true, None).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        received.try_recv().is_err(),
        "rejected calls reached a runtime"
    );

    for (uri, runtime, audience) in [
        (
            "/v1/model?agent_id=test-agent&audience_id=pinned",
            "test-runtime",
            Some("pinned"),
        ),
        ("/v1/model?agent_id=test-agent", "replacement", None),
        (
            "/v1/model?agent_id=test-agent&audience_id=fresh",
            "replacement",
            Some("fresh"),
        ),
    ] {
        let (status, body) = model_call(&state, "GET", uri, true, None).await?;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["runtime"], runtime, "{uri}");
        let (called, call) = received.try_recv()?;
        assert_eq!(called, runtime);
        assert_eq!(call["auth"], "Bearer test-only");
        let mut expected = json!({"interface_id": "test-interface", "agent_id": "test-agent"});
        if let Some(audience) = audience {
            expected["audience_id"] = json!(audience);
        }
        assert_eq!(call["query"], expected, "{uri}");
    }

    let update = json!({
        "agent_id": "test-agent",
        "audience_id": "pinned",
        "scope": "conversation",
        "selection": {"provider": "fixture", "model": "missing", "options": {}},
    });
    let (status, body) = model_call(&state, "PUT", "/v1/model", true, Some(&update)).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "That model is not available.");
    let (called, call) = received.try_recv()?;
    assert_eq!(called, "test-runtime");
    assert_eq!(call["update"]["interface_id"], "test-interface");
    assert_eq!(call["update"]["selection"], update["selection"]);
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn command_catalogues_come_from_the_conversation_runtime() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = model_fixture(directory.path()).await?;
    let (status, _) = model_call(
        &state,
        "GET",
        "/v1/commands?agent_id=test-agent&audience_id=pinned",
        false,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(received.try_recv().is_err());
    let (status, body) = model_call(
        &state,
        "GET",
        "/v1/commands?agent_id=test-agent&audience_id=pinned",
        true,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["runtime"], "test-runtime");
    assert_eq!(body["commands"], json!([{"name": "compact"}]));
    let (called, call) = received.try_recv()?;
    assert_eq!(called, "test-runtime");
    assert_eq!(
        call["query"],
        json!({"interface_id": "test-interface", "agent_id": "test-agent", "audience_id": "pinned"})
    );
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn model_calls_describe_unavailable_runtimes_for_people() -> Result<()> {
    let directory = tempdir()?;
    let (mut state, mut received, mut services) = model_fixture(directory.path()).await?;
    state.config.value.providers[1].enabled = false;
    let (status, body) = model_call(
        &state,
        "GET",
        "/v1/model?agent_id=test-agent&audience_id=pinned",
        true,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "conversation runtime provider is disabled");
    services.shutdown().await;
    let (status, body) =
        model_call(&state, "GET", "/v1/model?agent_id=test-agent", true, None).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "The runtime is not reachable right now.");
    assert!(received.try_recv().is_err());
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
        .claim_turn("test-runtime")?
        .context("missing running turn")?;
    let queued = accept(&state, "queued", "other").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;

    let error = tokio::time::timeout(Duration::from_secs(5), serve(state.config.clone()))
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
    let server = tokio::spawn(serve(state.config.clone()));
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
        assert!(result.is_err(), "invalid credential was accepted");
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
        state.config.value.agent.resource_root.join("SYSTEM.md"),
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
async fn follow_up_turns_carry_only_the_runtime_cursor_and_cannot_be_forged() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let cursor = serde_json::json!({"message": "native-message", "after": 1});
    let answered = cursor.clone();
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        Ok(RuntimeTurnResponse {
            follow_up: Some(answered),
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
    // The follow-up turn runs next with the cursor, no input and no files of its own.
    let seen = Arc::new(std::sync::Mutex::new(None));
    let record = seen.clone();
    run_next_turn(&state, "test-runtime", async move |_, _, request| {
        *record
            .lock()
            .map_err(|_| TurnFailure::Uncertain(anyhow!("poisoned")))? = Some(request.clone());
        Ok(RuntimeTurnResponse {
            correlation_id: request.correlation_id,
            output: "the report".to_owned(),
            follow_up: None,
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
    assert_eq!(request.native, serde_json::json!({ "follow_up": cursor }));
    assert!(request.attachments.is_empty() && request.unanswered.is_empty());
    let report = state
        .core
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing report")?;
    assert_eq!(report.content, "the report");

    // An interface cannot pose as core's follow-up.
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", state.config.credential("test-interface")?).parse()?,
    );
    let refused = accept_event(
        State(configured(&state)),
        headers,
        Json(ExternalEvent {
            event_id: "forged".to_owned(),
            agent_id: state.config.value.agent.id.clone(),
            user_id: "test-user".to_owned(),
            audience_id: "conversation".to_owned(),
            delivery_target_id: "test-destination".to_owned(),
            feedback_target_id: "test-feedback".to_owned(),
            content: "hello".to_owned(),
            native: serde_json::json!({ "follow_up": {"message": "x", "after": 0} }),
            attachments: None,
        }),
    )
    .await;
    assert_eq!(
        refused.err().map(|error| error.status),
        Some(StatusCode::BAD_REQUEST)
    );
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
        super::run_next_turn(&worker, "test-runtime", async move |_, _, request| {
            let _ = started.send(request.correlation_id.clone());
            let _ = held.await;
            Ok(completed(request))
        })
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
    let root = &state.config.value.agent.resource_root;
    let saved = directory.path().join("saved-resources");
    fs::rename(root, &saved)?;
    for missing in [true, false] {
        if !missing {
            fs::write(root, "not a directory")?;
        }
        assert!(accept(&state, "two", "conversation").await.is_err());
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
        state.config.value.agent.resource_root.join("AGENTS.md"),
    )?;
    let duplicate = accept(&state, "one", "conversation").await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, first.turn_id);
    assert_eq!(duplicate.state, TurnState::Queued);
    assert!(accept(&state, "two", "conversation").await.is_err());
    assert_eq!(state.core.database.recent_turns(None, 10)?.len(), 1);
    assert!(
        state
            .core
            .database
            .event_receipt("other-interface", "one")?
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
    state.config.value.agent.runtime_provider = "replacement".to_owned();
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
                        native: serde_json::Value::Null,
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
async fn runtime_ownership_survives_default_changes() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mut replacement = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    replacement.id = "replacement".to_owned();
    fs::write(
        directory.path().join("credentials/replacement.token"),
        "test-only",
    )?;
    state.config.value.providers.push(replacement);
    let first = accept(&state, "one", "conversation").await?;
    state.config.value.agent.runtime_provider = "replacement".to_owned();
    state.core.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    let second = accept(&state, "two", "conversation").await?;
    let fresh = accept(&state, "fresh", "new-conversation").await?;
    run_next_turn(&state, "replacement", async |_, _, request| {
        assert_eq!(request.correlation_id, fresh.turn_id);
        Ok(completed(request))
    })
    .await?;
    for receipt in [&first, &second] {
        let inspected = state
            .core
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?;
        assert_eq!(inspected.providers[1]["id"], "test-runtime");
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, receipt.turn_id);
            Ok(completed(request))
        })
        .await?;
    }
    let third = accept(&state, "three", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, third.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.core.database.turn_counts()?.completed, 4);
    Ok(())
}

#[tokio::test]
async fn startup_drains_queued_nondefault_runtime_without_replaying_running_work() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "interrupted", "blocked").await?;
    state
        .core
        .database
        .claim_turn("test-runtime")?
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
    state.config.value.agent.runtime_provider = "replacement".to_owned();
    let fresh = accept(&state, "fresh", "new-conversation").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    drop(listener);
    // Exercise the real startup worker selection and recovery against reopened SQLite.
    let config = state.config.clone();
    services.spawn(async move { serve(config).await.map_err(std::io::Error::other) });
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut calls = Vec::new();
        for _ in 0..3 {
            calls.push(received.recv().await.context("runtime trace ended")?);
        }
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("replacement", fresh.turn_id.clone()),
                ("test-runtime", blocked.turn_id.clone()),
                ("test-runtime", queued.turn_id.clone())
            ]
        );
        loop {
            if state.core.database.turn_counts()?.completed == 3 {
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
        assert_eq!(
            state
                .core
                .database
                .inspect_turn(&blocked.turn_id)?
                .context("blocked turn missing")?
                .state,
            TurnState::Completed
        );
        assert!(received.try_recv().is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn queued_turns_keep_the_same_runtime_conversation() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let second = accept(&state, "two", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, first.turn_id);
        Ok(completed(request))
    })
    .await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, second.turn_id);
        assert_eq!(request.audience_id, "conversation");
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.core.database.turn_counts()?.completed, 2);
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

/// A runtime serving `/v1/turns` with a fixed failure response, counting calls.
async fn failing_runtime(
    state: &AppState,
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
    assert!(received.try_recv().is_err());
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
        .claim_turn("test-runtime")?
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
    assert!(
        matches!(&outcome, Err(TurnFailure::Explained(text)) if text == "The usage limit has been reached"),
        "{outcome:?}"
    );
    let response = call_runtime(
        &state.client,
        &runtime,
        "unused",
        state.core.runtime_request(&turn, Vec::new()),
    )
    .await
    .map_err(|failure| match failure {
        TurnFailure::Retry(error) | TurnFailure::Uncertain(error) => error,
        TurnFailure::Explained(explanation) => anyhow!(explanation),
    })?;
    assert_eq!(response.correlation_id, turn.turn_id);
    server.abort();
    assert!(
        server
            .await
            .err()
            .context("server unexpectedly finished")?
            .is_cancelled()
    );
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
    let refused = check(&state.config)
        .err()
        .context("accepted a schema this release does not know")?;
    assert!(
        format!("{refused:#}").contains("not the current Carillon format"),
        "{refused:#}"
    );
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

async fn activity_call(
    state: &AppState,
    provider: &str,
    token: Option<&str>,
) -> Result<(StatusCode, serde_json::Value)> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let router = Router::new()
        .route("/v1/activity", get(active_turns))
        .with_state(configured(state));
    let mut request = Request::builder()
        .uri("/v1/activity")
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
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

/// A runtime that records each activity request and answers as `mode` says.
fn activity_runtime(
    mode: Arc<std::sync::Mutex<Snapshot>>,
    sent: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
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
async fn foreign_turn(state: &mut AppState, directory: &Path) -> Result<EventReceipt> {
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
                native: serde_json::Value::Null,
                attachments: None,
            },
        )
        .await?)
}

/// Claims the next turn, which must be `turn_id`, and keeps it running until released.
async fn hold_running(
    state: &AppState,
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
async fn activity_lists_only_the_callers_active_turns_with_runtime_snapshots() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let mode = Arc::new(std::sync::Mutex::new(Snapshot::Current));
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.providers[1].url = format!("http://{}", listener.local_addr()?);
    let mut services = tokio::task::JoinSet::new();
    let runtime = activity_runtime(mode.clone(), sent);
    services.spawn(async move { axum::serve(listener, runtime).await });

    let finished = accept(&state, "finished", "earlier").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(completed(request))
    })
    .await?;
    let running = accept(&state, "running", "conversation").await?;
    let queued = accept(&state, "queued", "conversation").await?;
    let foreign = foreign_turn(&mut state, directory.path()).await?;
    let (release, worker) = hold_running(&state, "test-runtime", &running.turn_id).await?;

    let (status, body) = activity_call(&state, "test-interface", Some("test-only")).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([
            {
                "turn_id": running.turn_id,
                "state": "running",
                "target_id": "test-destination",
                "accepted_at": body[0]["accepted_at"],
                "activity": {
                    "phase": "tools",
                    "running": [{"tool": "bash", "detail": "cargo test", "started_at": 1}],
                    "recent": [],
                    "tool_calls": 1,
                },
            },
            {
                "turn_id": queued.turn_id,
                "state": "queued",
                "target_id": "test-destination",
                "accepted_at": body[1]["accepted_at"],
                "activity": null,
            },
        ])
    );
    assert!(body[0]["accepted_at"].as_i64().is_some_and(|time| time > 0));
    assert_ne!(body[0]["turn_id"], json!(finished.turn_id));
    // Only running turns are relayed, with the runtime credential, naming the caller.
    assert_eq!(
        received.try_recv()?,
        json!({
            "id": running.turn_id,
            "query": {"interface_id": "test-interface"},
            "auth": "Bearer test-only",
        })
    );
    assert!(received.try_recv().is_err());

    // Another interface sees its own work and nothing of this one's.
    let (_, body) = activity_call(&state, "other-interface", Some("other-only")).await?;
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    assert_eq!(body[0]["turn_id"], json!(foreign.turn_id));

    // A snapshot the runtime cannot supply is absent, never a shorter list.
    for snapshot in [Snapshot::Missing, Snapshot::Malformed, Snapshot::Hanging] {
        *mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        let started = std::time::Instant::now();
        let (status, body) = activity_call(&state, "test-interface", Some("test-only")).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["state"], "running");
        assert_eq!(body[0]["activity"], serde_json::Value::Null);
        assert_eq!(body.as_array().map(Vec::len), Some(2));
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
        let (status, _) = activity_call(&state, provider, token).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{provider} {token:?}");
    }

    let _ = release.send(());
    assert_eq!(worker.await??, WorkerStep::Advanced);
    let (_, body) = activity_call(&state, "test-interface", Some("test-only")).await?;
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    assert_eq!(body[0]["turn_id"], json!(queued.turn_id));
    Ok(())
}

#[tokio::test]
async fn activity_answers_promptly_when_several_runtimes_cannot_describe_their_turns() -> Result<()>
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
        state.config.value.agent.runtime_provider.clone_from(&id);
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
    let (status, body) = activity_call(&state, "test-interface", Some("test-only")).await?;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    let listed = body
        .as_array()
        .context("list")?
        .iter()
        .map(|turn| {
            (
                turn["turn_id"].as_str().map(str::to_owned),
                turn["activity"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        accepted
            .into_iter()
            .map(|turn| (Some(turn), serde_json::Value::Null))
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
