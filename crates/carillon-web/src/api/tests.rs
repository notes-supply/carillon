//! What the page asks beside its stream: a control of a turn, and the
//! runtime's commands.

use anyhow::Result;
use axum::{
    body::Body,
    http::{Method, StatusCode, header::CONTENT_TYPE},
};
use carillon_protocol::turns::ControlAction;
use serde_json::json;

use crate::{
    api::AWAY,
    fixture::{Answer, Fixture, bearer},
};

const CHAT: &str = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";

#[tokio::test]
async fn a_control_reaches_core_for_the_message_it_names_and_a_refusal_comes_back() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let event = "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa";
    let control = |event: &str, body: &str| {
        bearer(
            Method::POST,
            &format!("/api/messages/{event}/control"),
            &token,
        )
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_owned()))
    };
    for action in ["steer", "interrupt", "stop"] {
        let answer = fixture
            .send(control(event, &json!({ "action": action }).to_string())?)
            .await?;
        assert_eq!(answer.status, StatusCode::NO_CONTENT, "{action}");
        assert!(answer.body.is_empty());
    }
    let relayed: Vec<(String, ControlAction)> = fixture
        .core()
        .controls
        .iter()
        .map(|(event, control)| (event.clone(), control.action))
        .collect();
    assert_eq!(
        relayed,
        [
            (event.to_owned(), ControlAction::Steer),
            (event.to_owned(), ControlAction::Interrupt),
            (event.to_owned(), ControlAction::Stop),
        ]
    );
    let (shape, id) = (StatusCode::UNPROCESSABLE_ENTITY, StatusCode::BAD_REQUEST);
    for (event, body, refused) in [
        (event, "{\"action\":\"resume\"}", shape),
        (event, "{}", shape),
        ("runtime:schedule-1", "{\"action\":\"stop\"}", id),
        (
            "0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
            "{\"action\":\"stop\"}",
            id,
        ),
    ] {
        let answer = fixture.send(control(event, body)?).await?;
        assert_eq!(answer.status, refused, "{event} {body}");
        if refused == id {
            assert_eq!(
                String::from_utf8_lossy(&answer.body),
                "Invalid URL: a message is `m-` and a lowercase UUID",
                "{event}"
            );
        }
    }
    assert_eq!(
        fixture.core().controls.len(),
        3,
        "a malformed control reached core"
    );

    fixture.core().refusal = Some((StatusCode::CONFLICT, "That turn no longer waits."));
    let late = fixture
        .send(control(event, "{\"action\":\"stop\"}")?)
        .await?;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert_eq!(late.json()?, json!({"error": "That turn no longer waits."}));
    Ok(())
}

fn commands(agent: &str, audience: &str) -> String {
    format!("/api/conversations/{agent}/{audience}/commands")
}

#[tokio::test]
async fn commands_are_those_of_the_agent_the_conversation_names() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    for agent in ["assistant", "second"] {
        let listed = fixture.get(&commands(agent, CHAT), &token).await?;
        assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
        assert_eq!(
            listed.json()?,
            json!({"commands": [{
                "name": format!("skill:{agent}"),
                "description": format!("in {CHAT}"),
                "hint": CHAT,
            }]})
        );
    }

    fixture.core().refusal = Some((
        StatusCode::SERVICE_UNAVAILABLE,
        "The agent's runtime is disabled.",
    ));
    let unavailable = fixture.get(&commands("second", CHAT), &token).await?;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavailable.json()?, json!({ "error": AWAY }));
    Ok(())
}

#[tokio::test]
async fn commands_of_a_conversation_the_page_could_not_name_are_refused_before_core_is_asked()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let unknown = fixture.get(&commands("no%2Fbody", CHAT), &token).await?;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        unknown.json()?,
        json!({"error": "Carillon has no agent no/body."})
    );
    for malformed in [
        "0190F3A8-7B1C-7DEF-8A2B-3C4D5E6F7A8B",
        "0190f3a87b1c7def8a2b3c4d5e6f7a8b",
        "%7B0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b%7D",
        "urn:uuid:0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
        "web-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b",
        "assistant",
    ] {
        let answer = fixture
            .get(&commands("assistant", malformed), &token)
            .await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{malformed}");
        assert!(
            answer.text().contains("a conversation is a lowercase UUID"),
            "{malformed}: {}",
            answer.text()
        );
    }
    let unkeyed = fixture
        .get(&format!("/api/conversations/{CHAT}/commands"), &token)
        .await?;
    assert_eq!(unkeyed.status, StatusCode::NOT_FOUND);
    assert!(fixture.core().asked.is_empty(), "core was asked");
    Ok(())
}

async fn command_list(fixture: &Fixture, token: &str) -> Result<Answer> {
    fixture.get(&commands("assistant", CHAT), token).await
}

async fn stop(fixture: &Fixture, token: &str) -> Result<Answer> {
    let control = "/api/messages/m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa/control";
    let control = bearer(Method::POST, control, token)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from("{\"action\":\"stop\"}"))?;
    fixture.send(control).await
}

#[tokio::test]
async fn while_core_is_away_the_page_is_told_to_try_again_and_never_what_went_wrong() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    fixture.serving.abort();
    while !fixture.serving.is_finished() {
        tokio::task::yield_now().await;
    }
    for answer in [
        command_list(&fixture, &token).await?,
        stop(&fixture, &token).await?,
    ] {
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer.json()?, json!({ "error": AWAY }));
    }
    Ok(())
}

#[tokio::test]
async fn what_core_says_of_its_own_failure_stays_in_the_log() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    for (status, said) in [
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "read provider credential /home/owner/.carillon/credentials/runtime-pi.token",
        ),
        (
            StatusCode::BAD_GATEWAY,
            "The runtime gave an unusable answer.",
        ),
        (
            StatusCode::GATEWAY_TIMEOUT,
            "The runtime did not answer in time.",
        ),
    ] {
        fixture.core().refusal = Some((status, said));
        let refused = [
            command_list(&fixture, &token).await?,
            stop(&fixture, &token).await?,
        ];
        for answer in refused {
            assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE, "{status}");
            assert_eq!(answer.json()?, json!({ "error": AWAY }), "{status}");
        }
    }
    fixture.core().refusal = None;
    fixture.core().away_for_requests = usize::MAX;
    for answer in [
        command_list(&fixture, &token).await?,
        stop(&fixture, &token).await?,
    ] {
        assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer.json()?, json!({ "error": AWAY }));
    }
    Ok(())
}
