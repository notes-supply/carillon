//! The settings pages `carillon settings` reads: one for each conversation
//! of this interface, and none for anything else.

use anyhow::Result;
use axum::{
    body::Body,
    http::{Method, StatusCode, header::CONTENT_TYPE},
};
use carillon_protocol::settings::{SettingsRequest, Subject};
use serde_json::{Value, json};

use crate::fixture::{Fixture, conversation, operator};

const CHAT: &str = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";

fn channel(agent: &str) -> Subject {
    Subject::Channel {
        interface_id: "web".to_owned(),
        channel_id: CHAT.to_owned(),
        agent_id: agent.to_owned(),
    }
}

#[tokio::test]
async fn the_latest_conversations_are_listed_as_settings_pages() -> Result<()> {
    let fixture = Fixture::new().await?;
    {
        let mut core = fixture.core();
        for activity in (1..=25).rev() {
            let audience = format!("0190f3a8-7b1c-7def-8a2b-{activity:012}");
            core.conversations
                .push(conversation("assistant", &audience, activity));
        }
        core.conversations[0].title = String::new();
        // What `GET /v1/channels/{id}/settings` would not find is not listed.
        core.conversations
            .insert(1, conversation("assistant", "not-a-uuid", 100));
    }
    let listed = fixture
        .send(operator(Method::GET, "/v1/channels").body(Body::empty())?)
        .await?;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
    let pages = listed.json()?;
    let titles: Vec<&str> = pages
        .as_array()
        .map(|pages| {
            pages
                .iter()
                .filter_map(|page| page["title"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(titles.len(), 20, "a bounded list");
    assert_eq!(
        titles[..3],
        [
            "Untitled · assistant",
            "title 24 · assistant",
            "title 23 · assistant"
        ]
    );
    assert_eq!(
        pages[1],
        json!({
            "title": "title 24 · assistant",
            "subject": {
                "kind": "channel",
                "interface_id": "web",
                "channel_id": "0190f3a8-7b1c-7def-8a2b-000000000024",
                "agent_id": "assistant",
            },
            "interface": [],
            "runtime": {"state": "settings", "settings": []},
        })
    );
    assert_eq!(fixture.core().settings.len(), 20);
    Ok(())
}

#[tokio::test]
async fn a_conversations_page_is_read_and_changed_at_its_own_agents_runtime() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 4));
    let settings = format!("/v1/channels/{CHAT}/settings");
    let shown = fixture
        .send(operator(Method::GET, &settings).body(Body::empty())?)
        .await?;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.text());
    assert_eq!(shown.json()?["title"], "title 4 · second");
    assert_eq!(shown.json()?["subject"]["agent_id"], "second");

    let change = json!({
        "setting": "model",
        "value": {"kind": "one", "id": "local/echo"},
        "expected": "ab".repeat(32),
    });
    let put = |body: &Value| {
        operator(Method::PUT, &settings)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
    };
    let changed = fixture.send(put(&change)?).await?;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text());
    assert_eq!(changed.json()?["runtime"]["state"], "settings");
    let asked: Vec<SettingsRequest> = fixture.core().settings.clone();
    assert_eq!(
        asked,
        [
            SettingsRequest {
                subject: channel("second"),
                change: None,
            },
            SettingsRequest {
                subject: channel("second"),
                change: Some(serde_json::from_value(change.clone())?),
            },
        ]
    );

    // A runtime's refusal of a change keeps its status and its words. A
    // runtime that does not answer a read leaves the page, saying why.
    fixture.core().refusal = Some((StatusCode::CONFLICT, "This page changed; read it again."));
    let stale = fixture.send(put(&change)?).await?;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(
        stale.json()?,
        json!({"error": "This page changed; read it again."})
    );
    let unavailable = fixture
        .send(operator(Method::GET, &settings).body(Body::empty())?)
        .await?;
    assert_eq!(unavailable.status, StatusCode::OK);
    assert_eq!(
        unavailable.json()?["runtime"],
        json!({"state": "unavailable", "reason": "This page changed; read it again."})
    );
    Ok(())
}

#[tokio::test]
async fn a_channel_that_is_no_conversation_of_this_interface_is_not_found() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .core()
        .conversations
        .push(conversation("assistant", CHAT, 1));
    for channel in [
        "0190f3a8-7b1c-7def-8a2b-ffffffffffff",
        "300",
        "dm",
        "web-assistant",
        "0190F3A8-7B1C-7DEF-8A2B-3C4D5E6F7A8B",
    ] {
        let settings = format!("/v1/channels/{channel}/settings");
        for request in [
            operator(Method::GET, &settings).body(Body::empty())?,
            operator(Method::PUT, &settings)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"setting": "model", "value": null, "expected": "ab".repeat(32)})
                        .to_string(),
                ))?,
        ] {
            let answer = fixture.send(request).await?;
            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{channel}");
            assert!(answer.text().contains("is not a web conversation"));
        }
    }
    assert!(fixture.core().settings.is_empty(), "a runtime was asked");
    Ok(())
}
