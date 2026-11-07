use super::*;
use crate::config::DiscordRoute;
use anyhow::anyhow;

#[test]
fn participation_persistence_retries_without_bypassing_activation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let participation = Participation::load(path.clone())?;
    if let Err(failure) = participation.adopt("400", "300") {
        eprintln!("adoption persistence failed: {failure}");
        assert_eq!(participation.parent_route("400"), None);
        participation.adopt("400", "300")?;
    }
    if let Err(failure) = participation.remember_created("500", "500", "300") {
        eprintln!("creation persistence failed: {failure}");
        assert_eq!(participation.created_thread("500"), None);
        assert_eq!(participation.parent_route("500"), None);
        assert_eq!(participation.parent_route("400").as_deref(), Some("300"));
        participation.remember_created("500", "500", "300")?;
    }
    let recovered = Participation::load(path)?;
    assert_eq!(recovered.created_thread("500").as_deref(), Some("500"));
    let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let plan = resolve_message(
        &config,
        IncomingMessage {
            event_id: "unmentioned".to_owned(),
            guild_id: Some("100".to_owned()),
            user_id: "200".to_owned(),
            channel_id: "400".to_owned(),
            parent_channel_id: recovered.parent_route("400"),
            content: "continue without a mention".to_owned(),
            attachments: Vec::new(),
            reference: None,
        },
        serde_json::json!({}),
        &recovered.lock(),
    )
    .map_err(|error| anyhow!(error.message))?
    .context("lost adopted-thread activation")?;
    assert!(!plan.adopt_thread);
    assert!(!plan.create_thread);
    Ok(())
}

#[test]
fn failed_participation_write_leaves_memory_and_disk_unchanged() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let participation = Participation::load(path.clone())?;
    participation.adopt("400", "300")?;
    let original = fs::read(&path)?;
    let temporary = path.with_extension("json.tmp");
    fs::create_dir(&temporary)?;
    for result in [
        participation.adopt("401", "300"),
        participation.remember_created("500", "500", "300"),
    ] {
        let error = result
            .err()
            .context("persisted participation despite an obstructed write")?
            .downcast::<std::io::Error>()?;
        assert_eq!(error.kind(), std::io::ErrorKind::IsADirectory);
    }
    assert_eq!(participation.parent_route("401"), None);
    assert_eq!(participation.created_thread("500"), None);
    assert_eq!(fs::read(&path)?, original);
    fs::remove_dir(temporary)?;
    participation.adopt("401", "300")?;
    participation.remember_created("500", "500", "300")?;
    let recovered = Participation::load(path)?;
    assert_eq!(recovered.parent_route("401").as_deref(), Some("300"));
    assert_eq!(recovered.created_thread("500").as_deref(), Some("500"));
    Ok(())
}

#[test]
fn admitted_commands_and_components_carry_the_configured_markers() -> Result<()> {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.activity_markers.uncertain = String::new();
    config
        .activity_markers
        .models
        .insert("claude-".to_owned(), "<a:claude:123>".to_owned());
    for kind in [InteractionKind::Command, InteractionKind::Component] {
        let source = InteractionSource {
            kind,
            ..interaction("300", None)
        };
        let admitted =
            admit_interaction(&config, source).map_err(|failure| anyhow!(failure.message))?;
        assert!(admitted.interaction.markers.uncertain.is_empty());
        assert_eq!(
            admitted.interaction.markers.brand("claude-opus-5-5"),
            "<a:claude:123>"
        );
    }
    Ok(())
}

#[test]
fn interaction_admission_follows_routes_and_conversations() -> Result<()> {
    let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let admitted = admit_interaction(&channel, interaction("300", None))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(admitted.audience_id.as_deref(), Some("discord-100-300"));
    assert!(admitted.interaction.conversation);
    // Threads under a channel route are not conversations the agent joins.
    assert_eq!(
        admit_interaction(&channel, interaction("400", Some("300")))
            .err()
            .map(|failure| failure.status),
        Some(StatusCode::FORBIDDEN)
    );

    let thread = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let in_channel = admit_interaction(&thread, interaction("300", None))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(in_channel.audience_id, None);
    assert!(!in_channel.interaction.conversation);
    let in_thread = admit_interaction(&thread, interaction("400", Some("300")))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(in_thread.audience_id.as_deref(), Some("discord-100-400"));
    assert!(in_thread.interaction.conversation);
    assert_eq!(
        in_thread.origin,
        InteractionOrigin {
            guild_id: "100".to_owned(),
            user_id: "200".to_owned(),
            channel_id: "400".to_owned(),
            route_channel_id: "300".to_owned(),
        }
    );
    assert_eq!(in_channel.origin.route_channel_id, "300");

    // A submission is anchored on its marker like a message, with its intent.
    let event = submitted_event(
        "default",
        "discord-100-300",
        &admitted.origin,
        "901",
        "/compact",
        Intent::Command,
    );
    assert_eq!(event.event_id, "901");
    assert_eq!(event.feedback_target_id, "300:901");
    assert_eq!(event.native["intent"], "command");
    assert_eq!(event.native["origin"]["parent_channel_id"], Value::Null);
    assert_eq!(event.channel_id, "300");
    let in_thread_event = submitted_event(
        "default",
        "discord-100-400",
        &in_thread.origin,
        "902",
        "/compact",
        Intent::Command,
    );
    assert_eq!(in_thread_event.delivery_target_id, "400");
    assert_eq!(in_thread_event.channel_id, "300");

    // A channel no route names is admitted unrouted, outside any
    // conversation; a thread in it is not.
    let unrouted = admit_interaction(&thread, interaction("301", None))
        .map_err(|failure| anyhow!(failure.message))?;
    assert!(matches!(&unrouted.routing, Routing::Unrouted(id) if id == "301"));
    assert_eq!(unrouted.audience_id, None);
    assert!(!unrouted.interaction.conversation);
    assert_eq!(unrouted.origin.route_channel_id, "301");

    for refused in [
        interaction("400", Some("301")),
        InteractionSource {
            user_id: "999".to_owned(),
            ..interaction("301", None)
        },
        InteractionSource {
            guild_id: Some("101".to_owned()),
            ..interaction("301", None)
        },
        InteractionSource {
            user_id: "999".to_owned(),
            ..interaction("300", None)
        },
        InteractionSource {
            guild_id: Some("101".to_owned()),
            ..interaction("300", None)
        },
        InteractionSource {
            guild_id: None,
            ..interaction("300", None)
        },
    ] {
        let failure = admit_interaction(&thread, refused.clone())
            .err()
            .with_context(|| format!("admitted {refused:?}"))?;
        assert_eq!(failure.status, StatusCode::FORBIDDEN);
    }
    Ok(())
}

fn titled(content: &str, files: &[&str]) -> MessagePlan {
    MessagePlan {
        message_id: "500".to_owned(),
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        agent_id: "default".to_owned(),
        channel_id: "300".to_owned(),
        route_channel_id: "300".to_owned(),
        content: content.to_owned(),
        attachments: files
            .iter()
            .map(|name| SourceAttachment {
                id: "600".to_owned(),
                filename: (*name).to_owned(),
                size: 1,
                content_type: None,
                url: "https://cdn.discordapp.com/attachments/300/600/file".to_owned(),
            })
            .collect(),
        reference: None,
        quotes: Vec::new(),
        create_thread: true,
        adopt_thread: false,
        native: Value::Null,
    }
}

#[test]
fn thread_titles_preserve_short_text_and_fit_discord() {
    assert_eq!(
        thread_title(&titled("  Plan   the weekend  ", &[])),
        "Plan the weekend"
    );
    for content in ["a long discussion ".repeat(20), "🦀".repeat(150)] {
        let title = thread_title(&titled(&content, &[]));
        assert!(!title.trim().is_empty());
        assert!(title.chars().count() <= 100);
        assert_eq!(title, thread_title(&titled(&content, &[])));
    }
    // A file-only message is named after its first file, or generically.
    assert_eq!(
        thread_title(&titled("", &["holiday  photo.jpg", "b.txt"])),
        "holiday photo.jpg"
    );
    assert_eq!(thread_title(&titled("", &["  "])), "Attachment");
    assert!(
        thread_title(&titled("", &[&"x".repeat(300)]))
            .chars()
            .count()
            <= 100
    );
}

#[test]
fn file_only_messages_activate_like_text_and_carry_their_files() -> Result<()> {
    let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let participation = ParticipationState::default();
    let file = titled("", &["photo.png"]).attachments;
    let mut mentioned = request("300", None, "<@201>");
    mentioned.attachments.clone_from(&file);
    let plan = accepted(resolve_message(
        &config,
        mentioned,
        json!({}),
        &participation,
    ))?;
    assert_eq!(plan.content, "");
    assert_eq!(plan.attachments, file);
    // Files do not activate a mention route by themselves.
    let mut unmentioned = request("300", None, "");
    unmentioned.attachments.clone_from(&file);
    assert!(
        resolve_message(&config, unmentioned, json!({}), &participation)
            .map_err(|failure| anyhow!(failure.message))?
            .is_none()
    );
    // Neither words nor files: nothing to do.
    assert!(
        resolve_message(
            &config,
            request("300", None, "<@201>"),
            json!({}),
            &participation
        )
        .map_err(|failure| anyhow!(failure.message))?
        .is_none()
    );
    Ok(())
}

#[test]
fn mention_activation_strips_only_the_configured_bot() -> Result<()> {
    let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let ignored = resolve_message(
        &config,
        request("300", None, "hello"),
        Value::Null,
        &ParticipationState::default(),
    )
    .map_err(|failure| anyhow!(failure.message))?;
    assert!(ignored.is_none());

    let plan = accepted(resolve_message(
        &config,
        request("300", None, "<@201> hello <@999>"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert_eq!(plan.content, "hello <@999>");
    let resolved = resolved_message(plan, "400".to_owned());
    assert_eq!(resolved.conversation_id, "discord-100-400");
    assert_eq!(resolved.event.delivery_target_id, "400");
    Ok(())
}

#[test]
fn mention_thread_adoption_persists_activation_for_later_messages() -> Result<()> {
    let config = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let adoption = accepted(resolve_message(
        &config,
        request("400", Some("300"), "<@201> adopt"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert!(adoption.adopt_thread);

    let participation = ParticipationState {
        threads: BTreeMap::from([("400".to_owned(), "300".to_owned())]),
        ..ParticipationState::default()
    };
    let continuation = accepted(resolve_message(
        &config,
        request("400", Some("300"), "continue"),
        Value::Null,
        &participation,
    ))?;
    assert!(!continuation.adopt_thread);
    let resolved = resolved_message(continuation, "400".to_owned());
    assert_eq!(resolved.conversation_id, "discord-100-400");
    assert_eq!(
        resolved.event.channel_id, "300",
        "a thread's conversation belongs to its routed channel"
    );
    Ok(())
}

#[test]
fn always_thread_accepts_children_but_channel_route_does_not() -> Result<()> {
    let thread = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    let thread_message = accepted(resolve_message(
        &thread,
        request("400", Some("300"), "continue"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert!(!thread_message.create_thread);

    let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let refused = resolve_message(
        &channel,
        request("400", Some("300"), "must reject"),
        Value::Null,
        &ParticipationState::default(),
    )
    .err()
    .context("a channel route admitted a thread child")?;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let first = accepted(resolve_message(
        &channel,
        request("300", None, "first"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    let second = accepted(resolve_message(
        &channel,
        request("300", None, "second"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert_eq!(
        resolved_message(first, "300".to_owned()).conversation_id,
        resolved_message(second, "300".to_owned()).conversation_id
    );
    Ok(())
}

pub(crate) fn discord_config(
    activation: DiscordActivation,
    conversation: DiscordConversation,
) -> DiscordConfig {
    DiscordConfig {
        guild_id: "100".to_owned(),
        bot_user_id: "201".to_owned(),
        bot_token_file: None,
        allowed_user_ids: vec!["200".to_owned()],
        routes: vec![DiscordRoute {
            channel_id: "300".to_owned(),
            agent: "default".to_owned(),
            activation,
            conversation,
        }],
        direct_messages: None,
        typing: true,
        activity: true,
        activity_markers: crate::config::ActivityMarkers::default(),
        attachments: crate::config::AttachmentPolicy::default(),
        reactions: true,
        processing_emoji: "👀".to_owned(),
        delivered_emoji: "✅".to_owned(),
        failed_emoji: "❌".to_owned(),
        uncertain_emoji: "⚠️".to_owned(),
        steered_emoji: "↪️".to_owned(),
        stopped_emoji: "⏹️".to_owned(),
        steer_emoji: "⏩".to_owned(),
        interrupt_emoji: "⏭️".to_owned(),
        stop_emoji: "🛑".to_owned(),
        module: crate::config::ModuleSetting::default(),
    }
}

pub(crate) fn participation(directory: &std::path::Path) -> Result<std::sync::Arc<Participation>> {
    Ok(std::sync::Arc::new(Participation::load(
        directory.join("participation.json"),
    )?))
}

/// Participation kept beside `file`, for tests that never read it.
pub(crate) fn unread_participation(file: &std::path::Path) -> std::sync::Arc<Participation> {
    std::sync::Arc::new(Participation::empty(
        file.with_file_name("participation.json"),
    ))
}

fn accepted(
    resolution: std::result::Result<Option<MessagePlan>, InterfaceError>,
) -> Result<MessagePlan> {
    resolution
        .map_err(|failure| anyhow!(failure.message))?
        .context("message was unexpectedly ignored")
}

fn request(channel_id: &str, parent_channel_id: Option<&str>, content: &str) -> IncomingMessage {
    IncomingMessage {
        event_id: "400".to_owned(),
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: channel_id.to_owned(),
        parent_channel_id: parent_channel_id.map(str::to_owned),
        content: content.to_owned(),
        attachments: Vec::new(),
        reference: None,
    }
}

pub(crate) fn interaction(channel_id: &str, parent_channel_id: Option<&str>) -> InteractionSource {
    InteractionSource {
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: channel_id.to_owned(),
        parent_channel_id: parent_channel_id.map(str::to_owned),
        kind: InteractionKind::Command,
        name: "settings".to_owned(),
        values: Vec::new(),
        options: Value::Null,
    }
}

/// A message as Discord's gateway sends it, by its fields beside the
/// defaults of a plain text message.
fn gateway(fields: &Value) -> Result<Message> {
    let mut message = json!({
        "id": "500", "channel_id": "300", "guild_id": "100",
        "author": {"id": "200", "username": "sender", "discriminator": "0"},
        "content": "", "attachments": [], "embeds": [], "mention_everyone": false,
        "mention_roles": [], "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-11-05T15:00:00.000000+00:00",
    });
    for (key, value) in fields.as_object().context("fields are an object")? {
        message[key] = value.clone();
    }
    Ok(serde_json::from_value(message)?)
}

pub(crate) fn agent_reads(event: &ExternalEvent) -> String {
    [
        event.native[NATIVE_CONTEXT].as_str().unwrap_or_default(),
        &event.content,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn sent(id: &str, author: &Value, content: &str, files: &Value) -> Value {
    json!({
        "id": id, "channel_id": "300", "author": author, "content": content,
        "attachments": files, "embeds": [], "mention_everyone": false, "mention_roles": [],
        "mentions": [], "pinned": false, "tts": false, "type": 0,
        "timestamp": "2026-11-05T14:03:27.123000+00:00",
    })
}

#[test]
fn gateway_messages_carry_what_they_reply_to_or_forward() -> Result<()> {
    let bot = json!({"id": "201", "username": "carillon", "discriminator": "0", "bot": true});
    let ada = json!({"id": "202", "username": "ada", "global_name": "Ada", "discriminator": "0"});
    let file = json!([{"id": "600", "filename": "plan.pdf", "size": 3,
        "url": "https://cdn.discordapp.com/attachments/300/600/plan.pdf?hm=signed",
        "proxy_url": "https://media.discordapp.net/attachments/300/600/plan.pdf"}]);
    let reply = |to: Value| {
        gateway(&json!({
            "type": 19, "content": "agreed",
            "message_reference": {"type": 0, "channel_id": "300", "message_id": "499"},
            "referenced_message": to,
        }))
    };

    let received = IncomingMessage::received(&reply(sent("499", &bot, "Ship it?", &file))?, "201");
    assert_eq!(
        received.reference,
        Some(Reference {
            kind: ReferenceKind::Reply,
            author: Some(Author::Bot {
                message_id: Id::new(499)
            }),
            sent_at: "2026-11-05T14:03:27+00:00".to_owned(),
            forwarding: false,
            content: "Ship it?".to_owned(),
            files: vec!["plan.pdf".to_owned()],
        })
    );
    assert_eq!(received.content, "agreed");
    let received =
        IncomingMessage::received(&reply(sent("499", &ada, "lunch?", &json!([])))?, "201");
    assert_eq!(
        received.reference.and_then(|reference| reference.author),
        Some(Author::Person {
            id: "202".to_owned(),
            name: "Ada".to_owned()
        })
    );
    let deleted = reply(Value::Null)?;
    assert_eq!(IncomingMessage::received(&deleted, "201").reference, None);

    let forward = gateway(&json!({
        "message_reference": {"type": 1, "channel_id": "800", "message_id": "799"},
        "message_snapshots": [{"message": {
            "content": "the original words", "attachments": [], "embeds": [],
            "edited_timestamp": null, "type": 0,
            "timestamp": "2026-11-04T09:00:00.000000+00:00",
        }}],
    }))?;
    assert_eq!(
        IncomingMessage::received(&forward, "201").reference,
        Some(Reference {
            kind: ReferenceKind::Forward,
            author: None,
            sent_at: "2026-11-04T09:00:00+00:00".to_owned(),
            forwarding: false,
            content: "the original words".to_owned(),
            files: Vec::new(),
        })
    );
    let pinned = gateway(&json!({
        "type": 6,
        "message_reference": {"type": 0, "channel_id": "300", "message_id": "499"},
    }))?;
    assert_eq!(
        IncomingMessage::received(&pinned, "201").reference,
        None,
        "a pin notice replies to nothing"
    );
    Ok(())
}

#[test]
fn a_gateway_voice_message_is_admitted_with_its_audio_file() -> Result<()> {
    let voice = gateway(&json!({
        "flags": 8192,
        "attachments": [{
            "id": "700", "filename": "voice-message.ogg", "size": 9120,
            "content_type": "audio/ogg", "duration_secs": 4.2, "waveform": "AAAA",
            "url": "https://cdn.discordapp.com/attachments/300/700/voice-message.ogg?hm=signed",
            "proxy_url": "https://media.discordapp.net/attachments/300/700/voice-message.ogg",
        }],
    }))?;
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let plan = accepted(resolve_message(
        &config,
        IncomingMessage::received(&voice, "201"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert_eq!(plan.content, "");
    assert_eq!(plan.reference, None);
    assert_eq!(
        plan.attachments,
        [SourceAttachment {
            id: "700".to_owned(),
            filename: "voice-message.ogg".to_owned(),
            size: 9120,
            content_type: Some("audio/ogg".to_owned()),
            url: "https://cdn.discordapp.com/attachments/300/700/voice-message.ogg?hm=signed"
                .to_owned(),
        }]
    );
    Ok(())
}

#[test]
fn direct_messages_never_run_commands_even_when_admitted() -> Result<()> {
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.direct_messages = Some(crate::config::DirectMessages {
        agent: "default".to_owned(),
    });
    let refused = admit_interaction(
        &config,
        InteractionSource {
            guild_id: None,
            ..interaction("900", None)
        },
    )
    .err()
    .context("a direct message ran a command")?;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[test]
fn direct_messages_stay_one_conversation_across_restarts_and_admit_only_routed_allowlisted_people()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let mut config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    config.direct_messages = Some(crate::config::DirectMessages {
        agent: "default".to_owned(),
    });
    let message = IncomingMessage {
        guild_id: None,
        channel_id: "900".to_owned(),
        ..request("900", None, "hello")
    };
    let mut conversations = Vec::new();
    for _ in 0..2 {
        let participation = Participation::load(path.clone())?;
        let plan = accepted(admit_message(
            &config,
            &participation,
            message.clone(),
            Value::Null,
        ))?;
        let resolved = prepare_deterministic_conversation(&participation, plan)
            .map_err(|failure| anyhow!(failure.message))?;
        conversations.push(resolved.conversation_id);
    }
    assert_eq!(conversations, ["discord-dm-900", "discord-dm-900"]);
    let participation = Participation::load(path)?;
    let refusal = |config: &DiscordConfig, message: IncomingMessage| {
        admit_message(config, &participation, message, Value::Null)
            .err()
            .map(|failure| failure.status)
    };
    for refused in [
        IncomingMessage {
            user_id: "999".to_owned(),
            ..message.clone()
        },
        IncomingMessage {
            guild_id: Some("999".to_owned()),
            ..message.clone()
        },
    ] {
        assert_eq!(refusal(&config, refused), Some(StatusCode::FORBIDDEN));
    }
    config.allowed_user_ids.clear();
    assert_eq!(
        refusal(&config, message.clone()),
        Some(StatusCode::FORBIDDEN)
    );
    config.allowed_user_ids.push("200".to_owned());
    config.direct_messages = None;
    assert_eq!(refusal(&config, message), Some(StatusCode::FORBIDDEN));
    Ok(())
}

mod quotes;
