use super::*;

#[tokio::test]
async fn message_actions_refuse_channels_outside_the_guild_and_allowlisted_direct_messages()
-> anyhow::Result<()> {
    let person = |id: &str| json!({"id": id, "username": "person", "discriminator": "0"});
    let cases = [
        (
            "a direct message with someone not allowlisted",
            json!({"id": "2", "type": 1, "recipients": [person("4")]}),
        ),
        (
            "a group direct message with an allowlisted person",
            json!({"id": "2", "type": 3, "recipients": [person("3")]}),
        ),
        (
            "a group direct message with mixed recipients",
            json!({"id": "2", "type": 3, "recipients": [person("3"), person("4")]}),
        ),
        (
            "a direct message without recipients",
            json!({"id": "2", "type": 1}),
        ),
        (
            "a direct message with no recipient",
            json!({"id": "2", "type": 1, "recipients": []}),
        ),
        ("another guild's channel", channel_fixture("9")),
        (
            "another guild's thread",
            json!({"id": "2", "type": 11, "guild_id": "9", "parent_id": "8", "name": "outside",
                "thread_metadata": {"archived": false, "auto_archive_duration": 1440,
                    "archive_timestamp": "2026-01-01T00:00:00+00:00", "locked": false}}),
        ),
    ];
    for (case, channel) in cases {
        for operation in [
            json!({"action": "messages.list", "channel_id": "2"}),
            json!({"action": "messages.send", "channel_id": "2", "content": "hello", "reply_to": "7"}),
        ] {
            let server = Mock::new(vec![get("/api/v10/channels/2", channel.clone())]).await?;
            let refused = server
                .execute(operation)
                .await?
                .err()
                .with_context(|| format!("admitted {case}"))?;
            assert_eq!(refused.outcome, "rejected", "{case}");
            server.finished()?;
            assert_eq!(server.bodies()?.len(), 1, "{case}");
        }
    }
    for operation in [
        json!({"action": "messages.list", "user_id": "4"}),
        json!({"action": "messages.send", "user_id": "4", "content": "hello", "reply_to": "7"}),
    ] {
        let server = Mock::new(vec![]).await?;
        let refused = server
            .execute(operation)
            .await?
            .err()
            .context("admitted someone not allowlisted")?;
        assert_eq!(refused.outcome, "rejected");
        assert!(server.bodies()?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn a_reply_to_a_message_outside_the_destination_sends_nothing() -> anyhow::Result<()> {
    for (destination, first) in [
        (
            json!({"channel_id": "2"}),
            get("/api/v10/channels/2", channel_fixture("1")),
        ),
        (
            json!({"user_id": "3"}),
            Step {
                method: Method::POST,
                path: "/api/v10/users/@me/channels",
                status: StatusCode::OK,
                body: json!({"id": "2", "type": 1,
                    "recipients": [{"id": "3", "username": "person", "discriminator": "0"}]}),
            },
        ),
    ] {
        let server = Mock::new(vec![
            first,
            Step {
                method: Method::GET,
                path: "/api/v10/channels/2/messages/777",
                status: StatusCode::NOT_FOUND,
                body: json!({"code": 10008, "message": "Unknown Message"}),
            },
        ])
        .await?;
        let mut operation =
            json!({"action": "messages.send", "content": "hello", "reply_to": "777"});
        for (key, value) in destination.as_object().context("destination")? {
            operation[key] = value.clone();
        }
        let refused = server
            .execute(operation)
            .await?
            .err()
            .context("sent a reply to an unknown message")?;
        assert_eq!(refused.outcome, "rejected");
        server.finished()?;
        assert_eq!(server.bodies()?.len(), 2);
    }
    Ok(())
}

/// Sends `content` to person 3 through their direct message 900, as
/// message `id`, with `sender` as the tools add it.
async fn send_directly(id: &str, content: &str, sender: Value) -> anyhow::Result<Mock> {
    let server = Mock::new(vec![
        Step {
            method: Method::POST,
            path: "/api/v10/users/@me/channels",
            status: StatusCode::OK,
            body: json!({"id": "900", "type": 1,
                "recipients": [{"id": "3", "username": "person", "discriminator": "0"}]}),
        },
        Step {
            method: Method::POST,
            path: "/api/v10/channels/900/messages",
            status: StatusCode::OK,
            body: json!({"id": id, "channel_id": "900",
                "author": {"id": "201", "username": "carillon", "discriminator": "0", "bot": true},
                "content": content, "attachments": [], "embeds": [], "mention_everyone": false,
                "mention_roles": [], "mentions": [], "pinned": false, "tts": false, "type": 0,
                "timestamp": "2026-11-05T14:03:00.000000+00:00"}),
        },
    ])
    .await?;
    let mut operation = json!({"action": "messages.send", "user_id": "3", "content": content});
    if !sender.is_null() {
        operation["sender"] = sender;
    }
    let sent = server
        .execute(operation)
        .await?
        .map_err(|failure| anyhow::anyhow!("{failure}"))?;
    assert_eq!(
        sent["url"],
        format!("https://discord.com/channels/@me/900/{id}")
    );
    server.finished()?;
    Ok(server)
}

/// What person 3's direct message `message` brings the assistant.
fn answered(
    server: &Mock,
    message: crate::conversations::IncomingMessage,
) -> anyhow::Result<String> {
    let mut config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Channel,
    );
    config.allowed_user_ids = vec!["3".to_owned()];
    config.direct_messages = Some(crate::config::DirectMessages {
        agent: "assistant".to_owned(),
    });
    let plan =
        crate::conversations::admit_message(&config, &server.participation, message, Value::Null)
            .map_err(|failure| anyhow::anyhow!(failure.message))?
            .context("the direct message was ignored")?;
    Ok(crate::conversations::tests::agent_reads(
        &crate::conversations::prepare_deterministic_conversation(&server.participation, plan)
            .map_err(|failure| anyhow::anyhow!(failure.message))?
            .event,
    ))
}

fn direct_message(
    id: &str,
    reference: Option<crate::conversations::Reference>,
) -> crate::conversations::IncomingMessage {
    crate::conversations::IncomingMessage {
        event_id: id.to_owned(),
        guild_id: None,
        user_id: "3".to_owned(),
        channel_id: "900".to_owned(),
        parent_channel_id: None,
        content: "merge it".to_owned(),
        attachments: Vec::new(),
        reference,
    }
}

#[tokio::test]
async fn a_direct_message_sent_from_another_conversation_opens_the_persons_next_one()
-> anyhow::Result<()> {
    let server = send_directly(
        "700",
        "PR #12 is ready for review.",
        json!({"agent_id": "assistant", "channel_id": "300"}),
    )
    .await?;
    let quote = "You sent them this direct message at 2026-11-05T14:03:00+00:00 from <#300>:\n\
                 > PR #12 is ready for review.";
    assert_eq!(
        answered(&server, direct_message("800", None))?,
        format!("{quote}\n\nmerge it")
    );
    let reply = crate::conversations::Reference {
        kind: crate::conversations::ReferenceKind::Reply,
        author: Some(crate::conversations::Author::Bot {
            message_id: Id::new(700),
        }),
        sent_at: "2026-11-05T14:03:00+00:00".to_owned(),
        forwarding: false,
        content: "PR #12 is ready for review.".to_owned(),
        files: Vec::new(),
    };
    assert_eq!(
        answered(&server, direct_message("800", Some(reply)))?,
        "In reply to your message, sent 2026-11-05T14:03:00+00:00 from <#300>:\n\
         > PR #12 is ready for review.\n\
         \n\
         merge it",
        "a reply to it quotes it once"
    );
    assert_eq!(
        answered(&server, direct_message("600", None))?,
        "merge it",
        "a direct message sent before it does not quote it"
    );
    Ok(())
}

#[tokio::test]
async fn only_what_was_sent_outside_the_persons_own_conversation_is_quoted_there()
-> anyhow::Result<()> {
    let own = send_directly(
        "700",
        "Here is the file.",
        json!({"agent_id": "assistant", "channel_id": "900"}),
    )
    .await?;
    assert_eq!(answered(&own, direct_message("800", None))?, "merge it");

    let unknown = send_directly("700", "Lunch is here.", Value::Null).await?;
    assert_eq!(
        answered(&unknown, direct_message("800", None))?,
        "Another agent sent them this direct message at 2026-11-05T14:03:00+00:00:\n\
         > Lunch is here.\n\
         \n\
         merge it"
    );
    Ok(())
}

#[tokio::test]
async fn an_edited_or_deleted_direct_message_is_quoted_as_the_person_sees_it() -> anyhow::Result<()>
{
    let server = send_directly(
        "700",
        "Deploy production?",
        json!({"agent_id": "assistant", "channel_id": "300"}),
    )
    .await?;
    let dm = json!({"id": "900", "type": 1,
        "recipients": [{"id": "3", "username": "person", "discriminator": "0"}]});
    let mut edited = json!({"id": "700", "channel_id": "900",
        "author": {"id": "201", "username": "carillon", "discriminator": "0", "bot": true},
        "content": "Deploy staging?", "attachments": [], "embeds": [], "mention_everyone": false,
        "mention_roles": [], "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-11-05T14:03:00.000000+00:00"});
    edited["edited_timestamp"] = json!("2026-11-05T14:04:00.000000+00:00");
    server
        .pending
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .extend([
            get("/api/v10/channels/900", dm.clone()),
            Step {
                method: Method::PATCH,
                path: "/api/v10/channels/900/messages/700",
                status: StatusCode::OK,
                body: edited,
            },
        ]);
    server
        .execute(json!({"action": "messages.edit", "channel_id": "900",
            "message_id": "700", "content": "Deploy staging?"}))
        .await?
        .map_err(|failure| anyhow::anyhow!("{failure}"))?;
    assert_eq!(
        answered(&server, direct_message("800", None))?,
        "You sent them this direct message at 2026-11-05T14:03:00+00:00 from <#300>:\n\
         > Deploy staging?\n\
         \n\
         merge it",
        "an edit is quoted as edited"
    );
    server
        .pending
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned"))?
        .extend([
            get("/api/v10/channels/900", dm),
            Step {
                method: Method::DELETE,
                path: "/api/v10/channels/900/messages/700",
                status: StatusCode::NO_CONTENT,
                body: Value::Null,
            },
        ]);
    server
        .execute(json!({"action": "messages.delete", "channel_id": "900", "message_id": "700"}))
        .await?
        .map_err(|failure| anyhow::anyhow!("{failure}"))?;
    server.finished()?;
    assert_eq!(
        answered(&server, direct_message("800", None))?,
        "merge it",
        "a deleted message is not quoted"
    );
    Ok(())
}
