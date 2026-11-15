//! The wire shape of an observation: snapshots round-trip unchanged, a view
//! travels as a query string, and what neither may say is refused.
use anyhow::{Context as _, Result, ensure};
use carillon_protocol::observation::{
    ConversationKey, InvalidView, Limit, ObservedTurn, OpenView, Opened, Snapshot, View,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Fixtures {
    snapshots: Vec<Value>,
    impossible_turns: Vec<Impossible>,
    impossible_openings: Vec<Impossible>,
}

#[derive(Deserialize)]
struct Impossible {
    #[serde(alias = "progress", alias = "open")]
    value: Value,
    refused_for: String,
}

fn fixtures() -> Result<Fixtures> {
    Ok(serde_json::from_str(include_str!(
        "fixtures/observation.json"
    ))?)
}

/// Fails unless `T` refuses `value` for `reason`.
fn refused<T: DeserializeOwned>(value: &Value, reason: &str) -> Result<()> {
    let error = serde_json::from_value::<T>(value.clone())
        .err()
        .with_context(|| format!("accepted {value}"))?;
    ensure!(
        error.to_string().contains(reason),
        "{value} was refused for another reason: {error}"
    );
    Ok(())
}

#[test]
fn snapshots_keep_their_wire_shape() -> Result<()> {
    for snapshot in fixtures()?.snapshots {
        let parsed: Snapshot = serde_json::from_value(snapshot.clone())?;
        assert_eq!(serde_json::to_value(parsed)?, snapshot);
    }
    Ok(())
}

#[test]
fn a_snapshot_cannot_say_what_a_turn_or_an_opened_conversation_cannot_be() -> Result<()> {
    let fixtures = fixtures()?;
    for turn in fixtures.impossible_turns {
        let value = json!({
            "turn_id": "01a0fc1d-4a00-7000-8000-000000000001",
            "source": { "kind": "opened" },
            "accepted_at": 1_783_771_200,
            "progress": turn.value,
        });
        refused::<ObservedTurn>(&value, &turn.refused_for)?;
    }
    for opening in fixtures.impossible_openings {
        refused::<Opened>(&opening.value, &opening.refused_for)?;
    }
    Ok(())
}

fn key(agent: &str, audience: &str) -> Result<ConversationKey> {
    Ok(ConversationKey::new(agent.to_owned(), audience.to_owned())?)
}

#[test]
fn a_view_travels_as_a_query_string_with_fifty_items_where_it_names_no_number() -> Result<()> {
    let open = |audience: &str, turns: u16| -> Result<Option<OpenView>> {
        Ok(Some(OpenView {
            conversation: key("assistant", audience)?,
            turns: Limit::new(turns)?,
        }))
    };
    for (query, conversations, open, written) in [
        ("", 50, None, "conversations=50"),
        ("conversations=3", 3, None, "conversations=3"),
        (
            "agent_id=assistant&audience_id=web%20chat%2F1",
            50,
            open("web chat/1", 50)?,
            "conversations=50&agent_id=assistant&audience_id=web+chat%2F1&turns=50",
        ),
        (
            "turns=500&audience_id=discord-100-300&agent_id=assistant&conversations=500",
            500,
            open("discord-100-300", 500)?,
            "conversations=500&agent_id=assistant&audience_id=discord-100-300&turns=500",
        ),
    ] {
        let view = View {
            conversations: Limit::new(conversations)?,
            open,
        };
        assert_eq!(serde_urlencoded::from_str::<View>(query)?, view, "{query}");
        assert_eq!(serde_urlencoded::to_string(&view)?, written);
        assert_eq!(serde_urlencoded::from_str::<View>(written)?, view);
    }
    Ok(())
}

#[test]
fn a_view_is_refused_for_a_window_out_of_range_half_a_key_or_anything_else_it_names() -> Result<()>
{
    let window = InvalidView::Limit.to_string();
    let named = InvalidView::Key.to_string();
    for (query, reason) in [
        ("conversations=0", window.as_str()),
        ("conversations=501", &window),
        ("conversations=70000", "number too large"),
        ("conversations=many", "invalid digit"),
        ("agent_id=a&audience_id=b&turns=0", &window),
        ("agent_id=a&audience_id=b&turns=501", &window),
        ("agent_id=a", &named),
        ("audience_id=b", &named),
        ("agent_id=&audience_id=b", &named),
        ("agent_id=a&audience_id=", &named),
        (
            "turns=10",
            &InvalidView::TurnsWithoutConversation.to_string(),
        ),
        ("interface_id=another", "unknown field `interface_id`"),
        ("conversations=1&conversations=2", "duplicate field"),
    ] {
        let error = serde_urlencoded::from_str::<View>(query)
            .err()
            .with_context(|| format!("accepted {query}"))?;
        ensure!(
            error.to_string().contains(reason),
            "{query} was refused for another reason: {error}"
        );
    }
    assert_eq!(Limit::new(0), Err(InvalidView::Limit));
    assert_eq!(Limit::new(Limit::MAX).map(Limit::get), Ok(500));
    Ok(())
}
