use super::*;

/// A Discord module that asks core for the model state through its one
/// native call, then replies `answered` when core gave the state, or
/// `failed` when it did not.
fn calling_model(answered: &str, failed: &str) -> anyhow::Result<Vec<u8>> {
    let description = describing(&[definition("model")]);
    let describe_length =
        serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
    let request =
        serde_json::to_string(&carillon_discord_module::contract::HostRequest::ModelState)?;
    let answered = json!({"Reply": {
        "content": answered,
        "embeds": [{"type": "rich", "description": "Model settings received"}],
        "components": [],
    }})
    .to_string();
    let failed = replying(failed, &[]);
    let card = showing("A test card");
    let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let text = format!(
        r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (import "carillon" "take" (func $take (param i32 i32)))
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 5120) "{}")
  (data (i32.const 6144) "{}")
  (data (i32.const 7168) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (local $reply i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        ;; A card request begins {{"Card"
        (if (result i32) (i32.eq (i32.load8_u (i32.const 8194)) (i32.const 67))
          (then (global.set $output (i32.const 7168)) (i32.const {}))
          (else
            (local.set $reply (call $call (i32.const 4096) (i32.const {})))
            (call $take (i32.const 32768) (local.get $reply))
            ;; {{"ModelState": ...}} or {{"Error": ...}}
            (if (result i32) (i32.eq (i32.load8_u (i32.const 32770)) (i32.const 77))
              (then (global.set $output (i32.const 5120)) (i32.const {}))
              (else (global.set $output (i32.const 6144)) (i32.const {}))))))))
  (func (export "output") (result i32) (global.get $output)))"#,
        escape(&description),
        escape(&request),
        escape(&answered),
        escape(&failed),
        escape(&card),
        description.len(),
        card.len(),
        request.len(),
        answered.len(),
        failed.len(),
    );
    Ok(wat::parse_str(text)?)
}

/// A gateway `/model` command from an allowed user in route channel 300.
pub(super) fn model_command() -> anyhow::Result<twilight_gateway::Event> {
    use twilight_model::gateway::payload::incoming::InteractionCreate;

    let command = json!({
        "id": "1",
        "application_id": "2",
        "authorizing_integration_owners": {},
        "token": "t",
        "version": 1,
        "type": 2,
        "guild_id": "100",
        "channel": {"id": "300", "type": 0},
        "member": {
            "user": {"id": "200", "username": "u", "discriminator": "0", "avatar": null},
            "roles": [],
            "joined_at": null,
            "deaf": false,
            "mute": false,
            "flags": 0
        },
        "data": {"id": "3", "name": "model", "type": 1}
    });
    Ok(twilight_gateway::Event::InteractionCreate(Box::new(
        InteractionCreate(serde_json::from_value(command)?),
    )))
}

/// The gateway goes, as when the provider stops, after taking an
/// interaction that Discord has acknowledged and whose native call to core
/// is still running: the call's result is still part of the reply sent
/// before the provider returns.
#[tokio::test]
async fn an_acknowledged_interaction_is_answered_before_the_provider_returns() -> anyhow::Result<()>
{
    use axum::routing::patch;
    use tokio::sync::Notify;

    let directory = tempfile::tempdir()?;
    // Discord takes the acknowledgement and the reply at once.
    let acknowledged = Arc::new(Notify::new());
    let replies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let discord = Router::new()
        .route(
            "/api/v10/interactions/{id}/{token}/callback",
            post({
                let acknowledged = acknowledged.clone();
                move || async move {
                    acknowledged.notify_one();
                    StatusCode::NO_CONTENT
                }
            }),
        )
        .route(
            "/api/v10/webhooks/{application}/{token}/messages/@original",
            patch({
                let replies = replies.clone();
                move |Json(reply): Json<Value>| async move {
                    if let Ok(mut replies) = replies.lock() {
                        replies.push(reply);
                    }
                    Json(json!({}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let discord_address = listener.local_addr()?;
    let discord_server = tokio::spawn(async move { axum::serve(listener, discord).await });
    // Core answers the module's native call only when the test releases it.
    let (asked, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let core = Router::new().route(
        "/v1/model",
        get({
            let (asked, release) = (asked.clone(), release.clone());
            move || async move {
                asked.notify_one();
                release.notified().await;
                Json(json!({
                    "providers": [], "available": [],
                    "agent": {"selection": null, "overridden": false}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core_address = listener.local_addr()?;
    let core_server = tokio::spawn(async move { axum::serve(listener, core).await });
    let mut state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    std::fs::write(
        directory.path().join("release.wasm"),
        calling_model("model state received", "the native call failed")?,
    )?;
    state.module = Arc::new(Module::load(files(directory.path()), Source::Bundled).await?);
    connect(&mut state, app(&discord_address.to_string(), 2, 100));
    let event = model_command()?;
    let mut interactions = tokio::task::JoinSet::new();
    assert!(super::super::take_event(&state, event, None, &mut interactions).is_none());
    // No event follows: the gateway is gone. Discord has the acknowledgement,
    // and the module's native call is waiting for core.
    tokio::time::timeout(Duration::from_secs(5), acknowledged.notified()).await?;
    tokio::time::timeout(Duration::from_secs(5), asked.notified()).await?;
    let answering = super::super::answered(&mut interactions);
    tokio::pin!(answering);
    tokio::time::timeout(Duration::from_millis(200), &mut answering)
        .await
        .err()
        .context("the provider returned while an acknowledged interaction was still running")?;
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), answering).await?;
    let replies = replies
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .clone();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0]["content"], "model state received");
    assert_eq!(
        replies[0]["embeds"],
        json!([{"type": "rich", "description": "Model settings received"}])
    );
    assert_eq!(replies[0]["allowed_mentions"], json!({"parse": []}));
    discord_server.abort();
    core_server.abort();
    Ok(())
}

/// A resumed gateway session replays an interaction this provider already
/// answered: Discord takes no second acknowledgement, so the module never
/// runs it again and core is never asked.
#[tokio::test]
async fn a_replayed_interaction_discord_does_not_acknowledge_again_runs_nothing()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let replies = Arc::new(AtomicUsize::new(0));
    let discord = Router::new()
        .route(
            "/api/v10/interactions/{id}/{token}/callback",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"code": 40060, "message": "Interaction has already been acknowledged."})),
                )
            }),
        )
        .fallback({
            let replies = replies.clone();
            move || async move {
                replies.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let discord_address = listener.local_addr()?;
    let discord_server = tokio::spawn(async move { axum::serve(listener, discord).await });
    let asked = Arc::new(AtomicUsize::new(0));
    let core = Router::new().fallback({
        let asked = asked.clone();
        move || async move {
            asked.fetch_add(1, Ordering::SeqCst);
            StatusCode::NO_CONTENT
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core_address = listener.local_addr()?;
    let core_server = tokio::spawn(async move { axum::serve(listener, core).await });
    let mut state = discord_state(
        core_client(format!("http://{core_address}"))?,
        directory.path(),
    )
    .await?;
    std::fs::write(
        directory.path().join("release.wasm"),
        calling_model("model state received", "the native call failed")?,
    )?;
    state.module = Arc::new(Module::load(files(directory.path()), Source::Bundled).await?);
    connect(&mut state, app(&discord_address.to_string(), 2, 100));
    let mut interactions = tokio::task::JoinSet::new();
    let taken = super::super::take_event(&state, model_command()?, None, &mut interactions);
    assert!(
        taken.is_none(),
        "an interaction is not a message awaiting settlement"
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        super::super::answered(&mut interactions),
    )
    .await?;
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the module ran the replay");
    assert_eq!(replies.load(Ordering::SeqCst), 0, "the replay was answered");
    discord_server.abort();
    core_server.abort();
    Ok(())
}
