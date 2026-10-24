use super::{admitted, relay};
use crate::{
    client::test_client,
    config::{DiscordActivation, DiscordConversation},
    conversations::tests::discord_config,
};
use axum::{Json, Router, extract::Path, http::StatusCode, routing::post};
use carillon_protocol::turns::ControlAction;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use twilight_model::{channel::message::EmojiReactionType, gateway::GatewayReaction, id::Id};

/// A reaction `user` added in `guild` to message 400 of channel 300.
fn reaction(user: u64, guild: Option<u64>, emoji: EmojiReactionType) -> GatewayReaction {
    GatewayReaction {
        burst: false,
        burst_colors: Vec::new(),
        channel_id: Id::new(300),
        emoji,
        guild_id: guild.map(Id::new),
        member: None,
        message_author_id: None,
        message_id: Id::new(400),
        user_id: Id::new(user),
    }
}

fn unicode(name: &str) -> EmojiReactionType {
    EmojiReactionType::Unicode {
        name: name.to_owned(),
    }
}

#[test]
fn only_an_allowlisted_persons_control_in_the_guild_is_admitted() {
    let mut settings = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    for (case, reaction, wanted) in [
        (
            "steer",
            reaction(200, Some(100), unicode("⏩")),
            Some(ControlAction::Steer),
        ),
        (
            "interrupt",
            reaction(200, Some(100), unicode("⏭️")),
            Some(ControlAction::Interrupt),
        ),
        (
            "without its variation selector",
            reaction(200, Some(100), unicode("⏭")),
            Some(ControlAction::Interrupt),
        ),
        (
            "stop",
            reaction(200, Some(100), unicode("🛑")),
            Some(ControlAction::Stop),
        ),
        (
            "the bot's own",
            reaction(201, Some(100), unicode("🛑")),
            None,
        ),
        (
            "a person not allowlisted",
            reaction(202, Some(100), unicode("🛑")),
            None,
        ),
        (
            "another guild",
            reaction(200, Some(101), unicode("🛑")),
            None,
        ),
        ("a direct message", reaction(200, None, unicode("🛑")), None),
        (
            "an emoji no control names",
            reaction(200, Some(100), unicode("👍")),
            None,
        ),
        (
            "an outcome's emoji",
            reaction(200, Some(100), unicode("⏹️")),
            None,
        ),
    ] {
        assert_eq!(admitted(&settings, &reaction), wanted, "{case}");
    }
    // Controls follow the settings serving, a custom emoji by its ID.
    settings.stop_emoji = "halt:123456789012345678".to_owned();
    let custom = |id| EmojiReactionType::Custom {
        animated: false,
        id: Id::new(id),
        name: Some("renamed".to_owned()),
    };
    let stop = reaction(200, Some(100), custom(123_456_789_012_345_678));
    assert_eq!(admitted(&settings, &stop), Some(ControlAction::Stop));
    let other = reaction(200, Some(100), custom(123_456_789_012_345_679));
    assert_eq!(admitted(&settings, &other), None);
    assert_eq!(
        admitted(&settings, &reaction(200, Some(100), unicode("🛑"))),
        None
    );
}

#[tokio::test]
async fn an_admitted_control_asks_core_to_relay_it_for_the_message() -> anyhow::Result<()> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let recorded = asked.clone();
    let app = Router::new().route(
        "/v1/events/{event_id}/control",
        post(
            move |Path(event_id): Path<String>, Json(body): Json<Value>| {
                recorded
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((event_id, body));
                async { StatusCode::CONFLICT }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = test_client(format!("http://{address}"))?;
    relay(core.clone(), "400".to_owned(), ControlAction::Steer).await;
    relay(core, "400".to_owned(), ControlAction::Stop).await;
    server.abort();
    assert_eq!(
        *asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [
            ("400".to_owned(), json!({"action": "steer"})),
            ("400".to_owned(), json!({"action": "stop"})),
        ]
    );
    Ok(())
}

/// Every log line at warning or above, as an operator's log would show it.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Warnings {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn only_an_unexpected_refusal_of_a_control_is_a_warning() -> anyhow::Result<()> {
    let answers = Arc::new(Mutex::new(vec![
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::CONFLICT,
        StatusCode::NOT_FOUND,
    ]));
    let app = Router::new().route(
        "/v1/events/{event_id}/control",
        post(move || {
            let status = answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop()
                .unwrap_or(StatusCode::NO_CONTENT);
            async move { status }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let core = test_client(format!("http://{address}"))?;
    let warnings = Warnings::default();
    let writer = warnings.clone();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let logged = || {
        String::from_utf8_lossy(
            &warnings
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    };
    for _ in 0..2 {
        relay(core.clone(), "400".to_owned(), ControlAction::Steer).await;
    }
    assert_eq!(logged(), "");
    relay(core, "400".to_owned(), ControlAction::Steer).await;
    server.abort();
    assert!(
        logged().contains("a turn control was not taken"),
        "{}",
        logged()
    );
    Ok(())
}
