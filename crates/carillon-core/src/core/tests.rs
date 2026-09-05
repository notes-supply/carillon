use std::{fs, path::Path};

use anyhow::{Context as _, bail};
use axum::{response::IntoResponse as _, routing::post};
use tempfile::tempdir;

use super::*;
use crate::model::TurnState;

fn installation(root: &Path) -> Result<AppState> {
    use crate::config::{AgentConfig, Config, CoreConfig, Provider, hash_token};

    // Core tests have no Discord configuration or Pi installation. These
    // provider IDs and continuation values are opaque test fixtures.
    let resources = root.join("resources");
    fs::create_dir(&resources)?;
    fs::write(resources.join("SYSTEM.md"), "test instructions")?;
    fs::create_dir(root.join("credentials"))?;
    let providers = [
        ("test-interface", ProviderRole::Interface),
        ("test-runtime", ProviderRole::Runtime),
    ]
    .into_iter()
    .map(|(id, role)| {
        fs::write(
            root.join("credentials").join(format!("{id}.token")),
            "test-only",
        )?;
        Ok(Provider {
            id: id.to_owned(),
            role,
            version: "test".to_owned(),
            url: "http://127.0.0.1:1".to_owned(),
            token_hash: hash_token("test-only"),
            enabled: true,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    let value = Config {
        release: RELEASE_VERSION.to_owned(),
        data_dir: root.join("state"),
        core: CoreConfig {
            listen: "127.0.0.1:1".parse()?,
            operator_token_hash: hash_token("test-operator"),
        },
        providers,
        discord: None,
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
        database: Database::open(&config.value.data_dir.join("carillon.sqlite3"))?,
        config,
        client: Client::new(),
    })
}

async fn accept(state: &AppState, id: &str, audience: &str) -> Result<EventReceipt> {
    let mut headers = HeaderMap::new();
    headers.insert(PROVIDER_HEADER, "test-interface".parse()?);
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", state.config.credential("test-interface")?).parse()?,
    );
    let receipt = accept_event(
        State(state.clone()),
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
        continuation_ref: format!("native-{}", request.audience_id),
    }
}

#[test]
fn configuration_rejects_malformed_provider_endpoints_before_startup() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let path = directory.path().join("carillon.toml");
    for index in 0..state.config.value.providers.len() {
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
async fn occupied_listener_leaves_accepted_and_running_work_untouched() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "running", "busy").await?;
    state
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
            .database
            .inspect_turn(&running.turn_id)?
            .context("missing running turn")?
            .state,
        TurnState::Running
    );
    assert_eq!(
        state
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
    use crate::model::{DeliveryOutcome, DeliveryState};

    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let receipt = accept(&state, "one", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        Ok(completed(request))
    })
    .await?;
    let claim = state
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
async fn duplicate_events_share_one_accepted_turn_without_reexecution() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let duplicate = accept(&state, "one", "conversation").await?;
    assert_eq!(first.state, TurnState::Queued);
    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.turn_id, first.turn_id);
    assert_eq!(state.database.recent_turns(None, 10)?.len(), 1);
    assert!(
        state
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
        assert_eq!(request.continuation_ref, None);
        assert!(Path::new(&request.resources.root).starts_with(&snapshot));
        assert_ne!(
            fs::read_to_string(Path::new(&request.resources.root).join("SYSTEM.md"))
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
        .database
        .claim_delivery("test-interface", "instance")?
        .context("missing reply")?;
    assert_eq!(delivery.turn_id, first.turn_id);
    assert_eq!(delivery.content, "reply to one");
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
    assert_eq!(state.database.recent_turns(None, 10)?.len(), 1);
    assert!(
        state
            .database
            .event_receipt("other-interface", "one")?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn runtime_ownership_survives_default_changes_and_unavailable_owners() -> Result<()> {
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
    state.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    let second = accept(&state, "two", "conversation").await?;
    let fresh = accept(&state, "fresh", "new-conversation").await?;
    run_next_turn(&state, "replacement", async |_, _, request| {
        assert_eq!(request.correlation_id, fresh.turn_id);
        assert_eq!(request.continuation_ref, None);
        Ok(completed(request))
    })
    .await?;
    for (receipt, continuation) in [(&first, None), (&second, Some("native-conversation"))] {
        let inspected = state
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?;
        assert_eq!(inspected.providers[1]["id"], "test-runtime");
        run_next_turn(&state, "test-runtime", async |_, _, request| {
            assert_eq!(request.correlation_id, receipt.turn_id);
            assert_eq!(request.continuation_ref.as_deref(), continuation);
            Ok(completed(request))
        })
        .await?;
    }
    let third = accept(&state, "three", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, third.turn_id);
        assert_eq!(
            request.continuation_ref.as_deref(),
            Some("native-conversation")
        );
        Ok(completed(request))
    })
    .await?;
    state.config.value.providers[1].enabled = false;
    assert!(accept(&state, "disabled", "conversation").await.is_err());
    assert!(accept(&state, "one", "conversation").await?.duplicate);
    state.config.value.providers.remove(1);
    assert!(accept(&state, "removed", "conversation").await.is_err());
    assert_eq!(state.database.recent_turns(None, 10)?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn startup_drains_queued_nondefault_runtime_without_replaying_running_work() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "interrupted", "blocked").await?;
    state
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
        for _ in 0..2 {
            calls.push(received.recv().await.context("runtime trace ended")?);
        }
        calls.sort();
        assert_eq!(
            calls,
            vec![
                ("replacement", fresh.turn_id.clone()),
                ("test-runtime", queued.turn_id.clone())
            ]
        );
        loop {
            if state.database.turn_counts()?.completed == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state
                .database
                .inspect_turn(&running.turn_id)?
                .context("interrupted turn missing")?
                .state,
            TurnState::Uncertain
        );
        assert_eq!(
            state
                .database
                .inspect_turn(&blocked.turn_id)?
                .context("blocked turn missing")?
                .state,
            TurnState::Queued
        );
        assert!(received.try_recv().is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn queued_turns_use_their_predecessors_continuation() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let first = accept(&state, "one", "conversation").await?;
    let second = accept(&state, "two", "conversation").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, first.turn_id);
        assert_eq!(request.continuation_ref, None);
        Ok(completed(request))
    })
    .await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, second.turn_id);
        assert_eq!(
            request.continuation_ref.as_deref(),
            Some("native-conversation")
        );
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.database.turn_counts()?.completed, 2);
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
            .database
            .inspect_turn(&receipt.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Queued
    );
    assert!(
        state
            .database
            .claim_delivery("test-interface", "instance")?
            .is_none()
    );
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.correlation_id, receipt.turn_id);
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.database.turn_counts()?.completed, 1);
    Ok(())
}

#[tokio::test]
async fn ambiguity_holds_only_its_conversation_without_replaying() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let uncertain = accept(&state, "one", "blocked").await?;
    accept(&state, "two", "blocked").await?;
    run_next_turn(&state, "test-runtime", async |_, _, _| {
        Err(TurnFailure::Uncertain(anyhow!("response lost")))
    })
    .await?;
    assert_eq!(
        state
            .database
            .inspect_turn(&uncertain.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Uncertain
    );
    assert_eq!(
        run_next_turn(&state, "test-runtime", async |_, _, _| {
            Err(TurnFailure::Uncertain(anyhow!(
                "uncertain work or its successor was submitted"
            )))
        })
        .await?,
        WorkerStep::Idle
    );
    accept(&state, "three", "unrelated").await?;
    run_next_turn(&state, "test-runtime", async |_, _, request| {
        assert_eq!(request.audience_id, "unrelated");
        Ok(completed(request))
    })
    .await?;
    assert_eq!(state.database.turn_counts()?.completed, 1);
    assert_eq!(state.database.turn_counts()?.uncertain, 1);
    Ok(())
}

#[tokio::test]
async fn cancelled_execution_is_uncertain_after_reopening_while_queued_work_resumes() -> Result<()>
{
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let running = accept(&state, "one", "blocked").await?;
    accept(&state, "two", "blocked").await?;
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
    state.database = Database::open(&state.config.value.data_dir.join("carillon.sqlite3"))?;
    assert_eq!(state.database.recover_running_turns()?, 1);
    assert_eq!(
        state
            .database
            .inspect_turn(&running.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Uncertain
    );
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

#[tokio::test(start_paused = true)]
async fn persistence_retry_cannot_downgrade_a_committed_completion() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .database
        .claim_turn("test-runtime")?
        .context("missing claim")?;
    let response = completed(runtime_request(&state, &turn)?);
    let mut attempts = 0;
    persist_outcome(&turn, "test lost commit acknowledgement", || {
        attempts += 1;
        if attempts == 1 {
            state.database.complete_turn(&turn, &response)?;
            bail!("completion committed, but caller did not receive success");
        }
        state
            .database
            .mark_uncertain(&turn, "apparent persistence failure")
    })
    .await;
    assert_eq!(attempts, 2);
    assert_eq!(
        state
            .database
            .inspect_turn(&turn.turn_id)?
            .context("missing turn")?
            .state,
        TurnState::Completed
    );
    assert_eq!(
        state.database.continuation(&turn)?.as_deref(),
        Some("native-conversation")
    );
    assert_eq!(
        state
            .database
            .claim_delivery("test-interface", "instance")?
            .context("missing reply")?
            .content,
        "reply to one"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_http_boundary_classifies_and_validates_responses() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    let turn = state
        .database
        .claim_turn("test-runtime")?
        .context("missing claim")?;
    let mut runtime = state
        .config
        .provider("test-runtime", ProviderRole::Runtime)?
        .clone();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    runtime.url = format!("http://{}", listener.local_addr()?);
    let app = Router::new().route("/v1/turns", post(|Json(request): Json<RuntimeTurnRequest>| async move {
        match request.input.as_str() {
            "forbidden" => StatusCode::FORBIDDEN.into_response(),
            "server error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            "malformed" => "not json".into_response(),
            "mismatch" => Json(json!({"correlation_id": "other", "output": "reply", "continuation_ref": "native"})).into_response(),
            "empty continuation" => Json(json!({"correlation_id": request.correlation_id, "output": "reply", "continuation_ref": ""})).into_response(),
            _ => Json(completed(request)).into_response(),
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    // These responses carry no protocol proof of non-admission. This is not
    // a blanket rule for a future explicit rejected-before-execution response.
    for input in [
        "forbidden",
        "server error",
        "malformed",
        "mismatch",
        "empty continuation",
    ] {
        let mut request = runtime_request(&state, &turn)?;
        request.input = input.to_owned();
        let outcome = call_runtime(&state.client, &runtime, "unused", request).await;
        assert!(
            matches!(outcome, Err(TurnFailure::Uncertain(_))),
            "{input}: {outcome:?}"
        );
    }
    let response = call_runtime(
        &state.client,
        &runtime,
        "unused",
        runtime_request(&state, &turn)?,
    )
    .await
    .map_err(|failure| match failure {
        TurnFailure::Retry(error) | TurnFailure::Uncertain(error) => error,
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
            runtime_request(&state, &turn)?
        )
        .await,
        Err(TurnFailure::Retry(_))
    ));
    Ok(())
}
