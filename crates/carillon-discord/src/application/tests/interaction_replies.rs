use super::*;

#[tokio::test]
async fn refused_menu_emoji_is_retried_once_without_changing_the_reply() -> anyhow::Result<()> {
    for (badge, status, accepts_retry, attempts) in [
        (true, StatusCode::BAD_REQUEST, true, 2),
        (true, StatusCode::BAD_REQUEST, false, 2),
        (false, StatusCode::BAD_REQUEST, false, 1),
        (true, StatusCode::INTERNAL_SERVER_ERROR, false, 1),
    ] {
        let directory = tempfile::tempdir()?;
        let replies = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let discord = Router::new()
            .route(
                "/api/v10/interactions/{id}/{token}/callback",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/api/v10/webhooks/{application}/{token}/messages/@original",
                axum::routing::patch({
                    let replies = replies.clone();
                    move |Json(reply): Json<Value>| {
                        let attempt = if let Ok(mut replies) = replies.lock() {
                            replies.push(reply);
                            replies.len()
                        } else {
                            0
                        };
                        async move {
                            if attempt == 1 || !accepts_retry {
                                (
                                    status,
                                    Json(json!({"code": 50035, "message": "Invalid Form Body"})),
                                )
                            } else {
                                (StatusCode::OK, Json(json!({})))
                            }
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, discord).await });
        let mut state = discord_state(
            core_client("http://127.0.0.1:1".to_owned())?,
            directory.path(),
        )
        .await?;
        let mut option = json!({"label": "Claude", "value": "claude", "default": true});
        if badge {
            option["emoji"] = json!({"name": "claude", "id": "123", "animated": false});
        }
        let embed = json!({"type": "rich", "description": "<:claude:123> Model settings"});
        let component = json!({"type": 1, "components": [{"type": 3, "custom_id": "model:provider:agent", "disabled": false, "options": [option]}]});
        let reply = json!({"content": "", "embeds": [embed], "components": [component]});
        state.module = stub_module(directory.path(), &json!({"Reply": reply}).to_string()).await?;
        connect(&mut state, app(&address.to_string(), 2, 100));
        let mut interactions = tokio::task::JoinSet::new();
        assert!(
            super::super::take_event(&state, settings_command()?, None, &mut interactions)
                .is_none()
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            super::super::answered(&mut interactions),
        )
        .await?;
        let replies = replies
            .lock()
            .map_err(|_| anyhow::anyhow!("reply capture lock"))?;
        assert_eq!(replies.len(), attempts, "badge={badge}, status={status}");
        let mut expected = reply;
        expected["allowed_mentions"] = json!({"parse": []});
        assert_eq!(replies[0], expected);
        if attempts == 2 {
            expected["components"][0]["components"][0]["options"][0]
                .as_object_mut()
                .context("option object")?
                .remove("emoji");
            assert_eq!(replies[1], expected);
        }
        server.abort();
    }
    Ok(())
}
