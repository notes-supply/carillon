//! How a start goes before its gateway serves.

use super::*;

/// The Discord REST API a start asks first: the bot and its application.
fn discord_identity() -> Router {
    Router::new()
        .route(
            "/api/v10/users/@me",
            get(|| async {
                Json(
                    json!({"id": "201", "username": "carillon", "discriminator": "0",
                            "avatar": null, "bot": true, "mfa_enabled": false}),
                )
            }),
        )
        .route(
            "/api/v10/applications/@me",
            get(|| async {
                Json(json!({"id": "2", "name": "carillon", "description": "",
                            "bot_public": false, "bot_require_code_grant": false, "flags": 0,
                            "icon": null, "team": null, "verify_key": "key"}))
            }),
        )
}

/// Discord's REST API for a start: the bot and its application answer,
/// and registering the module's commands is refused.
async fn refusing_registration() -> anyhow::Result<std::net::SocketAddr> {
    let discord = discord_identity().fallback(|| async {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 50035, "message": "Invalid Form Body"})),
        )
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, discord).await });
    Ok(address)
}

/// A start that fails before its gateway serves, here because Discord
/// refuses the module's commands, leaves the recorded session to the next.
#[tokio::test]
async fn a_start_that_fails_before_its_gateway_keeps_the_recorded_session() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let discord = refusing_registration().await?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let record = crate::gateway::record_path(&crate::config::state_directory(
        &state.installation,
        &state.health.id,
    ));
    std::fs::create_dir_all(record.parent().context("no state directory")?)?;
    std::fs::write(&record, b"recorded")?;
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(discord.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let reconciled = DeliveryReconciliation {
        feedback_target_ids: Vec::new(),
    };
    let failure = crate::application::connect(
        state,
        listener,
        http,
        "test-only".to_owned(),
        reconciled,
        None,
    )
    .await
    .err()
    .context("the start served despite the refused commands")?;
    assert!(
        format!("{failure:#}").contains("register Discord guild commands"),
        "{failure:#}"
    );
    assert_eq!(std::fs::read(&record)?, b"recorded");
    Ok(())
}

/// A start shows the uncertain reaction on each message whose delivery an
/// earlier instance claimed and never finished, such as an answer whose
/// upload a crash cut off, and sends it nothing.
#[tokio::test]
async fn a_start_marks_what_an_earlier_instance_left_unfinished_uncertain() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = requests.clone();
    let discord = discord_identity()
        .route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            axum::routing::put(|| async { Json(json!([])) }),
        )
        .route(
            "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
            axum::routing::any({
                let requests = recorded.clone();
                move |method: Method,
                      axum::extract::Path((channel, message, emoji)): axum::extract::Path<(
                    String,
                    String,
                    String,
                )>| async move {
                    if let Ok(mut requests) = requests.lock() {
                        requests.push(format!("{method} {channel}:{message} {emoji}"));
                    }
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .fallback(move |method: Method, uri: Uri| async move {
            if let Ok(mut requests) = recorded.lock() {
                requests.push(format!("{method} {uri}"));
            }
            StatusCode::NO_CONTENT
        });
    let discord_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = discord_listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(discord_listener, discord).await });
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let config = state.settings.current();
    let uncertain = format!("PUT 300:400 {}", config.uncertain_emoji);
    let processing = format!("DELETE 300:400 {}", config.processing_emoji);
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let starting = tokio::spawn(crate::application::connect(
        state,
        tokio::net::TcpListener::bind("127.0.0.1:0").await?,
        http,
        "test-only".to_owned(),
        DeliveryReconciliation {
            feedback_target_ids: vec!["300:400".to_owned()],
        },
        None,
    ));
    let seen = |request: &str| {
        requests
            .lock()
            .is_ok_and(|requests| requests.iter().any(|seen| seen == request))
    };
    let shown = tokio::time::timeout(Duration::from_secs(10), async {
        while !seen(&uncertain) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    starting.abort();
    server.abort();
    shown.context("no uncertain reaction was shown")?;
    assert!(seen(&processing), "the processing reaction is removed");
    let requests = requests.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
    assert!(
        !requests
            .iter()
            .any(|request| request.ends_with("/channels/300/messages")),
        "{requests:?}"
    );
    Ok(())
}
