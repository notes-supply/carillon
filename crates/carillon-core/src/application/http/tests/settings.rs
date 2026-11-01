use super::*;

type Calls = tokio::sync::mpsc::UnboundedReceiver<(&'static str, Value)>;

/// A runtime that records every settings and command call. A change to
/// `stale` is refused as a real runtime phrases it, and a change to `slow`
/// is never answered.
fn fake_runtime(
    id: &'static str,
    sent: tokio::sync::mpsc::UnboundedSender<(&'static str, Value)>,
) -> Router {
    fn bearer(headers: &HeaderMap) -> Option<&str> {
        headers.get(http::header::AUTHORIZATION)?.to_str().ok()
    }
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
            "/v1/settings",
            post(move |headers: HeaderMap, Json(request): Json<Value>| {
                let sent = sent.clone();
                async move {
                    let _ = sent.send((id, json!({"auth": bearer(&headers), "request": request})));
                    match request["change"]["setting"].as_str() {
                        Some("stale") => (
                            StatusCode::CONFLICT,
                            Json(json!({"error": "Thinking changed meanwhile."})),
                        )
                            .into_response(),
                        Some("slow") => std::future::pending().await,
                        _ => Json(json!({"runtime": id, "settings": []})).into_response(),
                    }
                }
            }),
        )
}

/// Two fake runtimes: `test-runtime` owns the `pinned` conversation and
/// `replacement` runs the agent now.
async fn relay_fixture(
    directory: &Path,
) -> Result<(Fixture, Calls, tokio::task::JoinSet<std::io::Result<()>>)> {
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
    state.config.value.agents[0].runtime = "replacement".to_owned();
    Ok((state, received, services))
}

/// Who calls: an interface with its own credential, or the operator.
#[derive(Clone, Copy, Debug)]
enum Caller {
    Interface(&'static str),
    Operator(&'static str),
}

const INTERFACE: Caller = Caller::Interface("test-only");
const OPERATOR: Caller = Caller::Operator("test-operator");

async fn call(
    state: &Fixture,
    caller: Caller,
    method: &str,
    uri: &str,
    body: Option<&[u8]>,
) -> Result<(StatusCode, Value)> {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    let router = Router::new()
        .route("/v1/settings", post(settings))
        .route("/v1/commands", get(commands))
        .with_state(configured(state));
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json");
    let token = match caller {
        Caller::Interface(token) => {
            request = request.header(PROVIDER_HEADER, "test-interface");
            token
        }
        Caller::Operator(token) => token,
    };
    request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_vec()));
    let response =
        tokio::time::timeout(Duration::from_secs(10), router.oneshot(request.body(body)?))
            .await??;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 65536).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

async fn settings_call(
    state: &Fixture,
    caller: Caller,
    request: &Value,
) -> Result<(StatusCode, Value)> {
    let body = serde_json::to_vec(request)?;
    call(state, caller, "POST", "/v1/settings", Some(&body)).await
}

fn channel(interface: &str, agent: &str) -> Value {
    json!({"kind": "channel", "interface_id": interface, "channel_id": "300", "agent_id": agent})
}

fn change(setting: &str) -> Value {
    json!({"setting": setting, "value": {"kind": "one", "id": "high"}, "expected": "a".repeat(64)})
}

#[tokio::test]
async fn settings_reach_the_runtime_that_runs_the_subjects_agent_now() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = relay_fixture(directory.path()).await?;
    for (caller, request) in [
        (
            INTERFACE,
            json!({"subject": channel("test-interface", "test-agent")}),
        ),
        (
            INTERFACE,
            json!({"subject": channel("test-interface", "test-agent"), "change": change("model.thinking")}),
        ),
        (
            OPERATOR,
            json!({"subject": {"kind": "agent", "agent_id": "test-agent"}}),
        ),
        (
            OPERATOR,
            json!({"subject": channel("another-interface", "test-agent")}),
        ),
    ] {
        let (status, body) = settings_call(&state, caller, &request).await?;
        assert_eq!(
            (status, body),
            (
                StatusCode::OK,
                json!({"runtime": "replacement", "settings": []})
            ),
            "{caller:?} {request}"
        );
        let (runtime, relayed) = received.try_recv()?;
        assert_eq!(runtime, "replacement");
        assert_eq!(
            relayed,
            json!({"auth": "Bearer test-only", "request": request})
        );
    }
    let (status, body) = settings_call(
        &state,
        OPERATOR,
        &json!({"subject": {"kind": "agent", "agent_id": "unknown"}}),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "Carillon has no agent unknown.");
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_interface_reads_and_changes_only_its_own_channels() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = relay_fixture(directory.path()).await?;
    let own = json!({"subject": channel("test-interface", "test-agent")});
    for (caller, request) in [
        (
            INTERFACE,
            json!({"subject": {"kind": "agent", "agent_id": "test-agent"}}),
        ),
        (
            INTERFACE,
            json!({"subject": channel("another-interface", "test-agent")}),
        ),
        (
            INTERFACE,
            json!({"subject": channel("another-interface", "test-agent"), "change": change("model")}),
        ),
        (Caller::Interface("wrong"), own.clone()),
        (Caller::Interface("test-operator"), own.clone()),
        (Caller::Operator("test-only"), own.clone()),
    ] {
        let (status, _) = settings_call(&state, caller, &request).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{caller:?} {request}");
    }
    // A body is read only once its caller is known.
    for (caller, status) in [
        (Caller::Operator("wrong"), StatusCode::FORBIDDEN),
        (OPERATOR, StatusCode::BAD_REQUEST),
        (INTERFACE, StatusCode::BAD_REQUEST),
    ] {
        let (answered, _) = call(&state, caller, "POST", "/v1/settings", Some(b"{}")).await?;
        assert_eq!(answered, status, "{caller:?}");
    }
    assert_eq!(
        received.try_recv(),
        Err(TryRecvError::Empty),
        "refused calls reached a runtime"
    );
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_runtimes_refusal_passes_through_and_its_silence_times_out() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = relay_fixture(directory.path()).await?;
    let subject = channel("test-interface", "test-agent");
    let (status, body) = settings_call(
        &state,
        INTERFACE,
        &json!({"subject": subject, "change": change("stale")}),
    )
    .await?;
    assert_eq!(
        (status, body),
        (
            StatusCode::CONFLICT,
            json!({"error": "Thinking changed meanwhile."})
        )
    );
    received.try_recv()?;
    let started = std::time::Instant::now();
    let (status, body) = settings_call(
        &state,
        INTERFACE,
        &json!({"subject": subject, "change": change("slow")}),
    )
    .await?;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["error"], "The runtime did not answer in time.");
    assert!(
        started.elapsed() >= RELAY_TIMEOUT,
        "{:?}",
        started.elapsed()
    );
    received.try_recv()?;
    services.shutdown().await;
    let (status, body) = settings_call(&state, INTERFACE, &json!({"subject": subject})).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "The runtime is not reachable right now.");
    Ok(())
}

#[tokio::test]
async fn a_disabled_runtime_is_not_called_for_settings() -> Result<()> {
    let directory = tempdir()?;
    let (mut state, mut received, mut services) = relay_fixture(directory.path()).await?;
    state.config.value.providers[2].enabled = false;
    let (status, body) = settings_call(
        &state,
        OPERATOR,
        &json!({"subject": {"kind": "agent", "agent_id": "test-agent"}}),
    )
    .await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "The agent's runtime is disabled.");
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn command_catalogues_come_from_the_current_conversation_runtime() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = relay_fixture(directory.path()).await?;
    let uri = "/v1/commands?agent_id=test-agent&channel_id=300&audience_id=fresh";
    let (status, _) = call(&state, Caller::Interface("wrong"), "GET", uri, None).await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    let (status, body) = call(&state, INTERFACE, "GET", uri, None).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["runtime"], "replacement");
    assert_eq!(body["commands"], json!([{"name": "compact"}]));
    let (called, call) = received.try_recv()?;
    assert_eq!(called, "replacement");
    assert_eq!(
        call,
        json!({
            "auth": "Bearer test-only",
            "query": {
                "interface_id": "test-interface",
                "agent_id": "test-agent",
                "channel_id": "300",
                "audience_id": "fresh",
            },
        })
    );
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn command_calls_refuse_stranded_conversations_before_transport() -> Result<()> {
    let directory = tempdir()?;
    let (mut state, mut received, mut services) = relay_fixture(directory.path()).await?;
    for availability in ["enabled", "disabled", "removed", "removed agent"] {
        let explanation = if availability == "removed agent" {
            "This conversation ran on test-runtime, which no longer runs test-agent."
        } else {
            "This conversation ran on test-runtime, which no longer runs test-agent. Your next message starts a new conversation on replacement."
        };
        match availability {
            "disabled" => state.config.value.providers[1].enabled = false,
            "removed" => {
                state.config.value.providers.remove(1);
            }
            "removed agent" => state.config.value.agents.clear(),
            _ => {}
        }
        let uri = "/v1/commands?agent_id=test-agent&channel_id=300&audience_id=pinned";
        let (status, body) = call(&state, INTERFACE, "GET", uri, None).await?;
        assert_eq!(status, StatusCode::CONFLICT, "{availability}");
        assert_eq!(body["error"], explanation);
        assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    }
    services.shutdown().await;
    Ok(())
}
