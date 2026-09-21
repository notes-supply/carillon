use super::*;
use crate::config::DiscordRoute;
use anyhow::anyhow;

#[test]
fn participation_persistence_retries_without_bypassing_activation() -> anyhow::Result<()> {
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
fn failed_participation_write_leaves_memory_and_disk_unchanged() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("participation.json");
    let participation = Participation::load(path.clone())?;
    participation.adopt("400", "300")?;
    let original = std::fs::read(&path)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::create_dir(&temporary)?;
    assert!(participation.adopt("401", "300").is_err());
    assert!(participation.remember_created("500", "500", "300").is_err());
    assert_eq!(participation.parent_route("401"), None);
    assert_eq!(participation.created_thread("500"), None);
    assert_eq!(std::fs::read(&path)?, original);
    std::fs::remove_dir(temporary)?;
    participation.adopt("401", "300")?;
    participation.remember_created("500", "500", "300")?;
    let recovered = Participation::load(path)?;
    assert_eq!(recovered.parent_route("401").as_deref(), Some("300"));
    assert_eq!(recovered.created_thread("500").as_deref(), Some("500"));
    Ok(())
}

#[test]
fn interaction_admission_follows_routes_and_conversations() -> anyhow::Result<()> {
    let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let admitted = admit_interaction(&channel, interaction("300", None))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(admitted.audience_id.as_deref(), Some("discord-100-300"));
    assert!(admitted.interaction.conversation);
    // Threads under a channel route are not conversations the agent joins.
    assert!(admit_interaction(&channel, interaction("400", Some("300"))).is_err());

    let thread = discord_config(DiscordActivation::Mention, DiscordConversation::Thread);
    let in_channel = admit_interaction(&thread, interaction("300", None))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(in_channel.audience_id, None);
    assert!(!in_channel.interaction.conversation);
    let in_thread = admit_interaction(&thread, interaction("400", Some("300")))
        .map_err(|failure| anyhow!(failure.message))?;
    assert_eq!(in_thread.audience_id.as_deref(), Some("discord-100-400"));
    assert!(in_thread.interaction.conversation);

    for refused in [
        interaction("301", None),
        interaction("400", Some("301")),
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

#[test]
fn thread_titles_preserve_short_text_and_fit_discord() {
    assert_eq!(thread_title("  Plan   the weekend  "), "Plan the weekend");
    for content in ["a long discussion ".repeat(20), "🦀".repeat(150)] {
        let title = thread_title(&content);
        assert!(!title.trim().is_empty());
        assert!(title.chars().count() <= 100);
        assert_eq!(title, thread_title(&content));
    }
}

#[test]
fn mention_activation_strips_only_the_configured_bot() -> anyhow::Result<()> {
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
fn mention_thread_adoption_persists_activation_for_later_messages() -> anyhow::Result<()> {
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
    assert_eq!(
        resolved_message(continuation, "400".to_owned()).conversation_id,
        "discord-100-400"
    );
    Ok(())
}

#[test]
fn always_thread_accepts_children_but_channel_route_does_not() -> anyhow::Result<()> {
    let thread = discord_config(DiscordActivation::Always, DiscordConversation::Thread);
    let thread_message = accepted(resolve_message(
        &thread,
        request("400", Some("300"), "continue"),
        Value::Null,
        &ParticipationState::default(),
    ))?;
    assert!(!thread_message.create_thread);

    let channel = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    assert!(
        resolve_message(
            &channel,
            request("400", Some("300"), "must reject"),
            Value::Null,
            &ParticipationState::default(),
        )
        .is_err()
    );
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
        agent: "default".to_owned(),
        guild_id: "100".to_owned(),
        bot_user_id: "201".to_owned(),
        allowed_user_ids: vec!["200".to_owned()],
        routes: vec![DiscordRoute {
            channel_id: "300".to_owned(),
            activation,
            conversation,
        }],
        typing: true,
        reactions: true,
        processing_emoji: "👀".to_owned(),
        delivered_emoji: "✅".to_owned(),
        failed_emoji: "❌".to_owned(),
        uncertain_emoji: "⚠️".to_owned(),
    }
}

fn accepted(
    resolution: std::result::Result<Option<MessagePlan>, InterfaceError>,
) -> anyhow::Result<MessagePlan> {
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
    }
}

pub(crate) fn interaction(channel_id: &str, parent_channel_id: Option<&str>) -> InteractionSource {
    InteractionSource {
        guild_id: Some("100".to_owned()),
        user_id: "200".to_owned(),
        channel_id: channel_id.to_owned(),
        parent_channel_id: parent_channel_id.map(str::to_owned),
        kind: InteractionKind::Command,
        name: "model".to_owned(),
        values: Vec::new(),
        options: Value::Null,
    }
}
