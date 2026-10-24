//! Deliveries with nothing to send, and answers with no message to show
//! feedback on.

use super::*;
use axum::body::Bytes;
use carillon_protocol::turns::Silence;

/// Runs the delivery loop over `deliveries` until core refuses the next
/// claim, and lets its reactions end. Returns every Discord request, by
/// method and path, and core's acknowledgements.
async fn deliver(
    deliveries: Vec<Delivery>,
) -> anyhow::Result<(Vec<String>, Vec<serde_json::Value>)> {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let pending = Arc::new(std::sync::Mutex::new(deliveries.into_iter()));
    let (seen, acknowledged) = (requests.clone(), acks.clone());
    let app = Router::new()
        .route(
            "/v1/deliveries",
            get(move || {
                let next = pending.lock().ok().and_then(|mut pending| pending.next());
                async move {
                    match next {
                        Some(delivery) => Json(Some(delivery)).into_response(),
                        None => StatusCode::FORBIDDEN.into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1/deliveries/{id}/ack",
            post(move |Json(ack): Json<serde_json::Value>| async move {
                acknowledged.lock().await.push(ack);
                StatusCode::NO_CONTENT
            }),
        )
        .fallback(move |method: Method, uri: Uri, body: Bytes| {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("{method} {}", uri.path()));
            let body = serde_json::from_slice(&body).unwrap_or_default();
            async move {
                if method == Method::POST {
                    Json(created(901, &body)).into_response()
                } else {
                    StatusCode::NO_CONTENT.into_response()
                }
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let settings = crate::config::Settings::new(discord_config(
        DiscordActivation::Always,
        DiscordConversation::Channel,
    ));
    let reactions = Reactions::new(http.clone(), settings);
    let (outcomes, _observed) = tokio::sync::mpsc::unbounded_channel();
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            reactions.clone(),
            http,
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await?;
    let ended = ended
        .err()
        .context("the loop ends only when core refuses a claim")?;
    assert!(
        ended.to_string().contains("delivery claim rejected"),
        "{ended:#}"
    );
    assert_eq!(reactions.stop(Duration::from_secs(5)).await, 0);
    server.abort();
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let acks = acks.lock().await.clone();
    Ok((requests, acks))
}

/// The bot's own reaction `emoji` on `message` in channel 300.
fn reaction(method: &str, message: &str, emoji: &str) -> String {
    use std::fmt::Write as _;
    let encoded = emoji.bytes().fold(String::new(), |mut encoded, byte| {
        let _ = write!(encoded, "%{byte:02X}");
        encoded
    });
    format!("{method} /api/v10/channels/300/messages/{message}/reactions/{encoded}/@me")
}

#[tokio::test]
async fn a_silent_delivery_sends_nothing_and_shows_how_its_turn_ended() -> anyhow::Result<()> {
    let steered = Delivery {
        content: String::new(),
        silence: Some(Silence::Steered),
        ..answer(1, TurnState::Completed)
    };
    let stopped = Delivery {
        content: String::new(),
        silence: Some(Silence::Stopped),
        feedback_target_id: "300:401".to_owned(),
        ..answer(2, TurnState::Interrupted)
    };
    let (requests, acks) = deliver(vec![steered, stopped]).await?;
    let mut requests = requests;
    requests.sort();
    let mut expected = vec![
        reaction("DELETE", "400", "👀"),
        reaction("PUT", "400", "↪️"),
        reaction("DELETE", "401", "👀"),
        reaction("PUT", "401", "⏹️"),
    ];
    expected.sort();
    assert_eq!(requests, expected);
    assert_eq!(
        acks,
        [
            json!({"native_ref": null, "outcome": "delivered"}),
            json!({"native_ref": null, "outcome": "delivered"}),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn an_answer_without_a_feedback_target_makes_no_reaction_calls() -> anyhow::Result<()> {
    let opened = Delivery {
        feedback_target_id: String::new(),
        ..answer(1, TurnState::Completed)
    };
    let (requests, acks) = deliver(vec![opened]).await?;
    assert_eq!(requests, ["POST /api/v10/channels/300/messages"]);
    assert_eq!(acks[0]["outcome"], "delivered");
    Ok(())
}
