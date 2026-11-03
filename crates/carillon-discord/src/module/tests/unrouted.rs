use super::*;

#[tokio::test]
async fn a_refused_settings_command_makes_no_core_call() -> anyhow::Result<()> {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let router = Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::SERVICE_UNAVAILABLE }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = crate::client::test_client(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    let module = release_module().await?;
    for (user, guild, reads) in [("999", "100", 0), ("200", "101", 0), ("200", "100", 1)] {
        let source = crate::conversations::InteractionSource {
            user_id: user.to_owned(),
            guild_id: Some(guild.to_owned()),
            ..crate::conversations::tests::interaction("300", None)
        };
        match crate::conversations::admit_interaction(&config, source) {
            Ok(admitted) => {
                let (_installation, writer) = crate::config::tests::writer()?;
                let calls = super::super::InteractionCalls::new(
                    core.clone(),
                    writer,
                    admitted.routing,
                    admitted.audience_id,
                );
                module.invoke(calls, admitted.interaction).await?;
            }
            Err(refusal) => assert_eq!(refusal.status, StatusCode::FORBIDDEN),
        }
        assert_eq!(
            asked.load(Ordering::SeqCst),
            reads,
            "{user} in guild {guild}"
        );
    }
    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_unrouted_channel_offers_its_agent_alone_and_reaches_no_runtime() -> anyhow::Result<()> {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let router = Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { StatusCode::SERVICE_UNAVAILABLE }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = crate::client::test_client(format!("http://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    let module = release_module().await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    for refused in [
        crate::conversations::InteractionSource {
            user_id: "999".to_owned(),
            ..crate::conversations::tests::interaction("301", None)
        },
        crate::conversations::InteractionSource {
            guild_id: Some("101".to_owned()),
            ..crate::conversations::tests::interaction("301", None)
        },
    ] {
        let refusal = crate::conversations::admit_interaction(&config, refused.clone())
            .err()
            .with_context(|| format!("admitted {refused:?}"))?;
        assert_eq!(refusal.status, StatusCode::FORBIDDEN);
    }
    let admitted = crate::conversations::admit_interaction(
        &config,
        crate::conversations::tests::interaction("301", None),
    )
    .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    let calls = super::super::InteractionCalls::new(
        core.clone(),
        writer,
        admitted.routing,
        admitted.audience_id,
    );
    let reply = module.invoke(calls, admitted.interaction).await?;
    assert_eq!(reply.content, "<#301>\n```\nAgent\n```");
    let offered: Vec<&str> = reply
        .components
        .iter()
        .filter_map(|row| row["components"].as_array())
        .flatten()
        .filter(|component| component["placeholder"] == "Agent")
        .filter_map(|component| component["options"].as_array())
        .flatten()
        .filter_map(|option| option["label"].as_str())
        .collect();
    assert_eq!(offered, ["default", "other"]);
    assert_eq!(asked.load(Ordering::SeqCst), 0, "a runtime was asked");
    server.abort();
    Ok(())
}
