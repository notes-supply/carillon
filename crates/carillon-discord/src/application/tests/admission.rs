//! What the gateway takes of a message, and from whom.

use super::*;
use twilight_gateway::Event;
use twilight_model::gateway::payload::incoming::{MessageCreate, MessageUpdate};

fn direct_message(author: &Value) -> anyhow::Result<MessageCreate> {
    Ok(serde_json::from_value(json!({
        "id": "500", "channel_id": "900", "author": author,
        "content": "hello", "attachments": [], "embeds": [], "mention_everyone": false,
        "mention_roles": [], "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-11-05T15:00:00+00:00",
    }))?)
}

#[tokio::test]
async fn the_gateway_takes_direct_messages_only_from_allowlisted_people_while_they_are_routed()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let state = discord_state(
        core_client("http://127.0.0.1:1".to_owned())?,
        directory.path(),
    )
    .await?;
    let mut interactions = tokio::task::JoinSet::new();
    let person = json!({"id": "200", "username": "sender", "discriminator": "0"});
    let outsider = json!({"id": "999", "username": "outsider", "discriminator": "0"});
    let created =
        |author| direct_message(&author).map(|message| Event::MessageCreate(Box::new(message)));
    let mut taken = |event| super::super::take_event(&state, event, None, &mut interactions);

    edit_settings(&state, |file| {
        let mut route = file.routes[0].clone();
        "900".clone_into(&mut route.channel_id);
        file.routes.push(route);
    })?;
    assert_eq!(
        settings(&state, Method::POST, "operator").await?.0,
        StatusCode::OK
    );
    assert!(
        taken(created(person.clone())?).is_none(),
        "a guild route naming the direct message's channel does not admit it"
    );

    edit_settings(&state, |file| {
        file.direct_messages = Some(crate::config::DirectMessages {
            agent: "default".to_owned(),
        });
        file.allowed_user_ids
            .extend(["201".to_owned(), "202".to_owned()]);
    })?;
    assert_eq!(
        settings(&state, Method::POST, "operator").await?.0,
        StatusCode::OK
    );
    assert!(taken(created(person.clone())?).is_some());
    assert!(taken(created(outsider)?).is_none());
    for id in ["201", "202"] {
        let bot = json!({"id": id, "username": "bot", "discriminator": "0", "bot": true});
        assert!(taken(created(bot)?).is_none(), "bot {id}");
    }
    let edited = Event::MessageUpdate(Box::new(MessageUpdate(direct_message(&person)?.0)));
    assert!(taken(edited).is_none());

    edit_settings(&state, |file| {
        file.allowed_user_ids.retain(|id| id != "200");
    })?;
    assert_eq!(
        settings(&state, Method::POST, "operator").await?.0,
        StatusCode::OK
    );
    assert!(
        taken(created(person)?).is_none(),
        "removed from the allowlist"
    );
    Ok(())
}

#[test]
fn native_messages_keep_attachment_metadata_without_signed_addresses() -> anyhow::Result<()> {
    let message: twilight_model::channel::Message = serde_json::from_value(json!({
        "id": "500", "channel_id": "300",
        "author": {"id": "200", "username": "sender", "discriminator": "0"},
        "content": "", "mention_everyone": false, "mention_roles": [],
        "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-01-01T00:00:00.000000+00:00",
        "attachments": [{
            "id": "600", "filename": "photo.png", "size": 10, "content_type": "image/png",
            "url": "https://cdn.discordapp.com/attachments/300/600/photo.png?ex=1&hm=signed",
            "proxy_url": "https://media.discordapp.net/attachments/300/600/photo.png?ex=1&hm=signed"
        }],
        "embeds": [{
            "type": "image",
            "url": "https://cdn.discordapp.com/attachments/300/602/shown.png?ex=1&hm=embedded",
            "image": {"url": "https://media.discordapp.net/attachments/300/602/shown.png?ex=1&hm=embedded"}
        }],
        // A reply carries the replied-to message, with its own attachments.
        "referenced_message": {
            "id": "499", "channel_id": "300",
            "author": {"id": "201", "username": "other", "discriminator": "0"},
            "content": "", "embeds": [], "mention_everyone": false, "mention_roles": [],
            "mentions": [], "pinned": false, "tts": false, "type": 0,
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "attachments": [{
                "id": "601", "filename": "earlier.pdf", "size": 20,
                "url": "https://cdn.discordapp.com/attachments/300/601/earlier.pdf?ex=1&hm=nested",
                "proxy_url": "https://media.discordapp.net/attachments/300/601/earlier.pdf?ex=1&hm=nested"
            }]
        }
    }))?;
    let native = super::super::native_message(&message);
    assert_eq!(native["attachments"][0]["filename"], "photo.png");
    assert_eq!(native["attachments"][0]["size"], 10);
    assert!(!native.to_string().contains("hm=signed"));
    assert!(!native.to_string().contains("hm=nested"));
    assert!(!native.to_string().contains("hm=embedded"));
    assert_eq!(
        native["embeds"][0]["url"],
        "https://cdn.discordapp.com/attachments/300/602/shown.png"
    );
    assert_eq!(
        native["referenced_message"]["attachments"][0]["filename"],
        "earlier.pdf"
    );
    assert!(native["attachments"][0].get("url").is_none());
    Ok(())
}
