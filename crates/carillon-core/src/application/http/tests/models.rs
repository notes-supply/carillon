use super::*;

/// A runtime that records every model call and rejects one selection the way
/// a real runtime phrases it for people.
fn fake_runtime(
    id: &'static str,
    sent: tokio::sync::mpsc::UnboundedSender<(&'static str, Value)>,
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
            move |headers: HeaderMap, Json(update): Json<Value>| {
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
    Fixture,
    tokio::sync::mpsc::UnboundedReceiver<(&'static str, Value)>,
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
    state.config.value.agents[0].runtime = "replacement".to_owned();
    Ok((state, received, services))
}

async fn model_call(
    state: &Fixture,
    method: &str,
    uri: &str,
    authorized: bool,
    body: Option<&Value>,
) -> Result<(StatusCode, Value)> {
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
    assert_eq!(
        received.try_recv(),
        Err(TryRecvError::Empty),
        "rejected calls reached a runtime"
    );

    for (uri, runtime, audience) in [
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
        "audience_id": "fresh",
        "scope": "conversation",
        "selection": {"provider": "fixture", "model": "missing", "options": {}},
    });
    let (status, body) = model_call(&state, "PUT", "/v1/model", true, Some(&update)).await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "That model is not available.");
    let (called, call) = received.try_recv()?;
    assert_eq!(called, "replacement");
    assert_eq!(call["update"]["interface_id"], "test-interface");
    assert_eq!(call["update"]["selection"], update["selection"]);
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn command_catalogues_come_from_the_current_conversation_runtime() -> Result<()> {
    let directory = tempdir()?;
    let (state, mut received, mut services) = model_fixture(directory.path()).await?;
    let (status, _) = model_call(
        &state,
        "GET",
        "/v1/commands?agent_id=test-agent&audience_id=fresh",
        false,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    let (status, body) = model_call(
        &state,
        "GET",
        "/v1/commands?agent_id=test-agent&audience_id=fresh",
        true,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["runtime"], "replacement");
    assert_eq!(body["commands"], json!([{"name": "compact"}]));
    let (called, call) = received.try_recv()?;
    assert_eq!(called, "replacement");
    assert_eq!(
        call["query"],
        json!({"interface_id": "test-interface", "agent_id": "test-agent", "audience_id": "fresh"})
    );
    services.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn model_calls_describe_unavailable_runtimes_for_people() -> Result<()> {
    let directory = tempdir()?;
    let (mut state, mut received, mut services) = model_fixture(directory.path()).await?;
    state.config.value.agents[0].runtime = "test-runtime".to_owned();
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
    state.config.value.providers[1].enabled = true;
    let (status, body) =
        model_call(&state, "GET", "/v1/model?agent_id=test-agent", true, None).await?;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "The runtime is not reachable right now.");
    assert_eq!(received.try_recv(), Err(TryRecvError::Disconnected));
    Ok(())
}

#[tokio::test]
async fn model_and_command_calls_refuse_stranded_conversations_before_transport() -> Result<()> {
    let directory = tempdir()?;
    let (mut state, mut received, mut services) = model_fixture(directory.path()).await?;
    for availability in ["enabled", "disabled", "removed"] {
        match availability {
            "disabled" => state.config.value.providers[1].enabled = false,
            "removed" => {
                state.config.value.providers.remove(1);
            }
            _ => {}
        }
        for uri in [
            "/v1/model?agent_id=test-agent&audience_id=pinned",
            "/v1/commands?agent_id=test-agent&audience_id=pinned",
        ] {
            let (status, body) = model_call(&state, "GET", uri, true, None).await?;
            assert_eq!(status, StatusCode::CONFLICT, "{availability} {uri}");
            assert_eq!(
                body["error"],
                "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
            );
        }
        for scope in ["conversation", "agent"] {
            let update = json!({"agent_id": "test-agent", "audience_id": "pinned", "scope": scope, "selection": null});
            let (status, body) =
                model_call(&state, "PUT", "/v1/model", true, Some(&update)).await?;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(
                body["error"],
                "This conversation ran on test-runtime, which no longer runs test-agent. Start a new conversation."
            );
        }
        assert_eq!(received.try_recv(), Err(TryRecvError::Empty));
    }
    services.shutdown().await;
    Ok(())
}
