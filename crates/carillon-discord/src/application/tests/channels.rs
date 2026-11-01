use super::*;

use carillon_protocol::settings::{Page, SettingsRequest};

async fn settings_core(
    refusal: Option<(StatusCode, &'static str)>,
) -> anyhow::Result<(
    CoreClient,
    Arc<std::sync::Mutex<Vec<SettingsRequest>>>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let router = Router::new().route(
        "/v1/settings",
        post(move |Json(request): Json<SettingsRequest>| {
            let recorded = recorded.clone();
            async move {
                let changing = request.change.is_some();
                if let Ok(mut requests) = recorded.lock() {
                    requests.push(request);
                }
                match refusal {
                    Some((status, error)) if changing => {
                        (status, Json(json!({ "error": error }))).into_response()
                    }
                    _ => Json(json!({"settings": [{
                        "id": "model.thinking", "name": "Thinking",
                        "choices": [{"id": "low", "name": "Low"}, {"id": "high", "name": "High"}],
                        "value": {"kind": "one", "id": "low"}, "inherited": false,
                        "revision": "a".repeat(64),
                    }]}))
                    .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((core_client(format!("http://{address}"))?, requests, server))
}

async fn operator(
    state: &DiscordState,
    method: Method,
    uri: &str,
    bearer: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))?;
    let response = discord_router(state.clone(), false)
        .oneshot(request)
        .await?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok((status, serde_json::from_slice(&body)?))
}

fn thinking(value: &str) -> Value {
    json!({
        "setting": "model.thinking",
        "value": {"kind": "one", "id": value},
        "expected": "a".repeat(64),
    })
}

#[tokio::test]
async fn the_operator_reads_every_route_page_and_only_the_operator() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, requests, server) = settings_core(None).await?;
    let state = discord_state(core, directory.path()).await?;
    for (method, uri, body) in [
        (Method::GET, "/v1/channels", None),
        (Method::GET, "/v1/channels/300/settings", None),
        (
            Method::PUT,
            "/v1/channels/300/settings",
            Some(thinking("high")),
        ),
    ] {
        let (status, _) = operator(&state, method, uri, "token", body).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(
        requests
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .is_empty()
    );

    let (status, body) = operator(&state, Method::GET, "/v1/channels", "operator", None).await?;
    assert_eq!(status, StatusCode::OK);
    let pages: Vec<Page> = serde_json::from_value(body)?;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].title, "#300");
    let (status, body) = operator(
        &state,
        Method::GET,
        "/v1/channels/300/settings",
        "operator",
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_value::<Page>(body)?, pages[0]);
    let (status, body) = operator(
        &state,
        Method::GET,
        "/v1/channels/301/settings",
        "operator",
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({"error": "Channel 301 is not a Discord route."})
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_operator_change_answers_the_page_or_the_refusal_as_it_came() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, requests, server) = settings_core(None).await?;
    let state = discord_state(core, directory.path()).await?;
    let (status, body) = operator(
        &state,
        Method::PUT,
        "/v1/channels/300/settings",
        "operator",
        Some(thinking("high")),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "#300");
    let sent = requests
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .clone();
    assert_eq!(sent.len(), 1, "a change is answered without another read");
    assert_eq!(
        serde_json::to_value(&sent[0].subject)?,
        json!({"kind": "channel", "interface_id": "discord", "channel_id": "300", "agent_id": "default"})
    );

    let route_line = json!({
        "setting": "listens",
        "value": {"kind": "one", "id": "mention"},
        "expected": "a".repeat(64),
    });
    let (status, body) = operator(
        &state,
        Method::PUT,
        "/v1/channels/300/settings",
        "operator",
        Some(route_line),
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body, json!({"error": "Listens is read-only."}));
    let (status, body) = operator(
        &state,
        Method::PUT,
        "/v1/channels/300/settings",
        "operator",
        Some(json!({"setting": "model.thinking"})),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    assert_eq!(
        requests
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .len(),
        1
    );
    server.abort();

    for (refusal, error) in [
        (StatusCode::CONFLICT, "This setting changed meanwhile."),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "high is not offered here.",
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The runtime is not reachable right now.",
        ),
        (
            StatusCode::GATEWAY_TIMEOUT,
            "The runtime did not answer in time.",
        ),
    ] {
        let (core, _, server) = settings_core(Some((refusal, error))).await?;
        let state = discord_state(core, directory.path()).await?;
        let (status, body) = operator(
            &state,
            Method::PUT,
            "/v1/channels/300/settings",
            "operator",
            Some(thinking("high")),
        )
        .await?;
        assert_eq!((status, body), (refusal, json!({ "error": error })));
        server.abort();
    }
    Ok(())
}

#[tokio::test]
async fn a_connected_provider_titles_pages_with_the_channels_name() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, _, core_server) = settings_core(None).await?;
    let mut state = discord_state(core, directory.path()).await?;
    let discord = Router::new().route(
        "/api/v10/channels/300",
        get(|| async { Json(json!({"id": "300", "type": 0, "name": "home"})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let discord_server = tokio::spawn(async move { axum::serve(listener, discord).await });
    connect(&mut state, app(&address.to_string(), 2, 100));
    let (status, body) = operator(
        &state,
        Method::GET,
        "/v1/channels/300/settings",
        "operator",
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "#home");
    discord_server.abort();
    core_server.abort();
    Ok(())
}
