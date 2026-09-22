use super::test_client as core_client;
use anyhow::Context as _;
use axum::{Json, Router, http::StatusCode, routing::get};
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
                let client = core_client(format!("http://{}", listener.local_addr()?));
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
                                delivery_target_id: "target".to_owned(),
                                feedback_target_id: "feedback".to_owned(),
                                content: "hello".to_owned(),
                                native: Value::Null,
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

#[tokio::test]
async fn model_calls_relay_rejections_in_the_runtime_words() -> anyhow::Result<()> {
    let app = Router::new().route(
        "/v1/model",
        get(|| async {
            (
                StatusCode::CONFLICT,
                Json(json!({"error": "The runtime provider is disabled."})),
            )
        })
        .put(|| async { (StatusCode::OK, "{not json") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = core_client(format!("http://{address}"));
    let query = carillon_protocol::model::ModelQuery {
        agent_id: "default".to_owned(),
        audience_id: None,
    };
    assert_eq!(
        core.model_state(&query).await.err().as_deref(),
        Some("The runtime provider is disabled.")
    );
    let update = carillon_protocol::model::ModelUpdate {
        agent_id: "default".to_owned(),
        audience_id: None,
        scope: carillon_protocol::model::ModelScope::Agent,
        selection: None,
        expected: None,
    };
    assert_eq!(
        core.set_model(&update).await.err().as_deref(),
        Some("The runtime reported an unusable model state.")
    );
    server.abort();
    let unreachable = core_client("http://127.0.0.1:1".to_owned());
    assert_eq!(
        unreachable.model_state(&query).await.err().as_deref(),
        Some("The runtime is not reachable right now.")
    );
    Ok(())
}
