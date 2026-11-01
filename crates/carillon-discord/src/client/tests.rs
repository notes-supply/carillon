use super::test_client as core_client;
use anyhow::Context as _;
use axum::{Json, Router, http::StatusCode};
use carillon_protocol::model::{DeliveryOutcome, ExternalEvent};
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test]
async fn core_requests_timeout_on_stalled_headers_and_bodies() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut cases = tokio::task::JoinSet::new();
    for operation in 0..4 {
        for partial_body in [false, true] {
            // Acknowledgements need only a success status, not a response body.
            if operation == 3 && partial_body {
                continue;
            }
            cases.spawn(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let client = core_client(format!("http://{}", listener.local_addr()?))?;
                let mut services = tokio::task::JoinSet::new();
                services.spawn(async move {
                    let (mut connection, _) = listener.accept().await?;
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let length = connection.read(&mut buffer).await?;
                        anyhow::ensure!(length > 0, "request disconnected early");
                        request.extend_from_slice(&buffer[..length]);
                    }
                    if partial_body {
                        connection
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                            .await?;
                    }
                    std::future::pending::<()>().await;
                    Ok::<_, anyhow::Error>(())
                });
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    match operation {
                        0 => client
                            .submit_event(&ExternalEvent {
                                event_id: "timeout".to_owned(),
                                agent_id: "agent".to_owned(),
                                user_id: "user".to_owned(),
                                audience_id: "conversation".to_owned(),
                                channel_id: "channel".to_owned(),
                                delivery_target_id: "target".to_owned(),
                                feedback_target_id: "feedback".to_owned(),
                                content: "hello".to_owned(),
                                native: Value::Null,
                                attachments: None,
                            })
                            .await
                            .map(|_| ()),
                        1 => client.claim_delivery().await.map(|_| ()),
                        2 => client.reconcile().await.map(|_| ()),
                        _ => {
                            client
                                .acknowledge_delivery(1, DeliveryOutcome::Delivered, None)
                                .await
                        }
                    }
                })
                .await?;
                let failure = result.err().context("stalled response was accepted")?;
                assert!(failure.is_transient());
                assert!(started.elapsed() >= Duration::from_secs(10));
                assert!(
                    services.try_join_next().is_none(),
                    "server failed instead of stalling"
                );
                Ok::<_, anyhow::Error>(())
            });
        }
    }
    let mut count = 0;
    while let Some(result) = cases.join_next().await {
        result??;
        count += 1;
    }
    assert_eq!(count, 7);
    Ok(())
}

/// Core's refusals keep their status and words, so a stale change, a value
/// not offered, a late runtime and an unreachable one stay apart. The fake
/// core answers with the status a request's channel names.
#[tokio::test]
async fn settings_calls_keep_each_refusal_apart() -> anyhow::Result<()> {
    use axum::{response::IntoResponse as _, routing::post};
    use carillon_protocol::settings::{SettingsRequest, Subject};

    let app = Router::new().route(
        "/v1/settings",
        post(|Json(request): Json<SettingsRequest>| async move {
            let Subject::Channel { channel_id, .. } = request.subject else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            match channel_id.as_str() {
                "200" => (StatusCode::OK, "{not json").into_response(),
                "late" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    StatusCode::OK.into_response()
                }
                status => (
                    status
                        .parse()
                        .ok()
                        .and_then(|status| StatusCode::from_u16(status).ok())
                        .unwrap_or(StatusCode::BAD_REQUEST),
                    Json(json!({"error": format!("refused with {status}")})),
                )
                    .into_response(),
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = core_client(format!("http://{address}"))?;
    let request = |channel: &str| SettingsRequest {
        subject: Subject::Channel {
            interface_id: "discord".to_owned(),
            channel_id: channel.to_owned(),
            agent_id: "default".to_owned(),
        },
        change: None,
    };
    for (channel, status, message) in [
        ("409", StatusCode::CONFLICT, "refused with 409"),
        ("422", StatusCode::UNPROCESSABLE_ENTITY, "refused with 422"),
        ("504", StatusCode::GATEWAY_TIMEOUT, "refused with 504"),
        ("503", StatusCode::SERVICE_UNAVAILABLE, "refused with 503"),
        (
            "200",
            StatusCode::BAD_GATEWAY,
            "The runtime gave an unusable answer.",
        ),
        ("late", StatusCode::GATEWAY_TIMEOUT, super::RUNTIME_LATE),
    ] {
        let failure = core
            .settings(&request(channel))
            .await
            .err()
            .with_context(|| format!("{channel} answered"))?;
        assert_eq!(
            (failure.status, failure.message.as_str()),
            (status, message)
        );
    }
    server.abort();
    let unreachable = core_client("http://127.0.0.1:1".to_owned())?;
    let failure = unreachable
        .settings(&request("300"))
        .await
        .err()
        .context("unreachable core answered")?;
    assert_eq!(failure.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failure.message, "The runtime is not reachable right now.");
    Ok(())
}
