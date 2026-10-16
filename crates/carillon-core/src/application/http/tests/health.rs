use super::*;

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
        .with_state(configured(&state));
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/v1/status").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        received.try_recv(),
        Err(TryRecvError::Empty),
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
async fn stalled_providers_are_reported_within_the_operator_timeout() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    fs::write(
        directory.path().join("credentials/operator.token"),
        "test-operator",
    )?;
    // Four enabled providers accept a health request and never answer it;
    // a disabled one sits among them. Keep slow fixtures away from backup's
    // 127.0.0.1 probes.
    let mut services = tokio::task::JoinSet::new();
    let mut providers = Vec::new();
    for index in 0..5 {
        let listener = TcpListener::bind("127.0.0.2:0").await?;
        let mut provider = state.config.value.providers[index % 2].clone();
        provider.id = format!("stalled-{index}");
        provider.role = ProviderRole::Interface;
        provider.url = format!("http://{}", listener.local_addr()?);
        provider.enabled = index != 2;
        let router = Router::new().route("/health", get(std::future::pending::<StatusCode>));
        services.spawn(async move { axum::serve(listener, router).await });
        providers.push(provider);
    }
    state.config.value.providers = providers;
    let listener = TcpListener::bind("127.0.0.2:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    let config = state.config.clone();
    let router = Router::new()
        .route("/v1/status", get(status))
        .with_state(configured(&state));
    services.spawn(async move { axum::serve(listener, router).await });

    // The operator's status request, bounded at five seconds, gets the
    // report, in registry order, each provider judged by its own timeout.
    let report = crate::application::operations::status(&config).await?;
    let shown = report
        .providers
        .iter()
        .map(|health| (health.id.as_str(), health.status.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        [
            ("core", "ready"),
            ("stalled-0", "unavailable"),
            ("stalled-1", "unavailable"),
            ("stalled-2", "disabled"),
            ("stalled-3", "unavailable"),
            ("stalled-4", "unavailable"),
        ]
    );
    // Readiness names the providers that did not answer.
    let waited = crate::application::operations::wait_until_healthy(&config, Duration::ZERO)
        .await
        .err()
        .context("stalled providers were healthy")?;
    assert!(
        format!("{waited:#}").contains("unavailable: stalled-0, stalled-1, stalled-3, stalled-4"),
        "{waited:#}"
    );
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
async fn idle_runtimes_are_not_probed_and_do_not_block_start_or_update_health() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    state.config.value.agents.clear();
    fs::write(
        directory.path().join("credentials/operator.token"),
        "test-operator",
    )?;
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut services = tokio::task::JoinSet::new();
    for provider in &mut state.config.value.providers {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        provider.url = format!("http://{}", listener.local_addr()?);
        let report = provider_status(provider, "ready");
        let sent = sent.clone();
        let router = Router::new().route(
            "/health",
            get(move || {
                let report = report.clone();
                let _ = sent.send(report.id.clone());
                async move { Json(report) }
            }),
        );
        services.spawn(async move { axum::serve(listener, router).await });
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    state.config.value.core.listen = listener.local_addr()?;
    let router = Router::new()
        .route("/v1/status", get(status))
        .with_state(configured(&state));
    services.spawn(async move { axum::serve(listener, router).await });
    let config = &state.config;
    let report = crate::application::operations::status(config).await?;
    assert_eq!(report.providers[2].id, "test-runtime");
    assert_eq!(report.providers[2].status, "idle");
    let healthy =
        crate::application::operations::wait_until_healthy(config, Duration::ZERO).await?;
    assert_eq!(healthy[2].status, "idle");
    assert_eq!(received.try_recv()?, "test-interface");
    assert_eq!(received.try_recv()?, "test-interface");
    assert_eq!(
        received.try_recv(),
        Err(TryRecvError::Empty),
        "idle runtime was probed"
    );
    Ok(())
}
