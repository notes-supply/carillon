//! Sending: the identities a message gets, what a retry does, and what the
//! page is told when a message cannot be taken.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
};
use carillon_protocol::model::{StagedAttachments, StagedFile};
use serde_json::json;
use tokio::sync::Notify;

use crate::fixture::{Answer, Fixture, bearer, conversation};

const CHAT: &str = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";
const EVENT: &str = "m-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa";
const BOUNDARY: &str = "carillon-test-boundary";
const MIB: usize = 1024 * 1024;

enum Part<'a> {
    Text(&'a str),
    File(&'a str, Option<&'a str>, &'a [u8]),
}

fn form(parts: &[Part<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::Text(text) => {
                body.extend_from_slice(b"Content-Disposition: form-data; name=\"text\"\r\n\r\n");
                body.extend_from_slice(text.as_bytes());
            }
            Part::File(name, content_type, bytes) => {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n"
                    )
                    .as_bytes(),
                );
                if let Some(content_type) = content_type {
                    body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
                }
                body.extend_from_slice(b"\r\n");
                body.extend_from_slice(bytes);
            }
        }
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn message(token: &str, path: &str, body: Vec<u8>) -> Result<Request<Body>> {
    Ok(bearer(Method::PUT, path, token)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))?)
}

fn path(event: &str, query: &str) -> String {
    format!("/api/conversations/{CHAT}/messages/{event}{query}")
}

async fn send(fixture: &Fixture, token: &str, query: &str, parts: &[Part<'_>]) -> Result<Answer> {
    fixture
        .send(message(token, &path(EVENT, query), form(parts))?)
        .await
}

/// The batches still in the provider's staging directory.
fn staged(fixture: &Fixture) -> Result<usize> {
    let incoming = fixture
        .directory
        .path()
        .join("state/components/web/incoming");
    Ok(std::fs::read_dir(incoming)?.count())
}

#[tokio::test]
async fn a_first_message_starts_the_conversation_with_every_identity_made_here() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let picture = b"\x89PNG\r\n\x1a\npicture";
    let sent = send(
        &fixture,
        &token,
        "?agent=second&command=true",
        &[
            Part::Text("look at these"),
            Part::File("chart \u{56f3}.png", Some("image/png"), picture),
            Part::File("", None, b"notes"),
        ],
    )
    .await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json()?,
        json!({"turn_id": format!("turn-{EVENT}"), "duplicate": false})
    );
    let core = fixture.core();
    let [(event, files)] = core.events.as_slice() else {
        anyhow::bail!("expected one event, not {}", core.events.len());
    };
    assert_eq!(
        (
            event.event_id.as_str(),
            event.agent_id.as_str(),
            event.user_id.as_str(),
            event.audience_id.as_str(),
            event.channel_id.as_str(),
            event.delivery_target_id.as_str(),
            event.feedback_target_id.as_str(),
            event.content.as_str(),
        ),
        (
            EVENT,
            "second",
            "owner",
            CHAT,
            CHAT,
            CHAT,
            EVENT,
            "look at these"
        )
    );
    assert_eq!(event.native, json!({"intent": "command"}));
    let StagedAttachments {
        batch,
        files: named,
    } = event.attachments.as_ref().context("no attachments")?;
    assert_eq!(
        named,
        &[
            StagedFile {
                name: "chart \u{56f3}.png".to_owned(),
                size: picture.len() as u64,
                content_type: Some("image/png".to_owned()),
            },
            // A part with no name gets one.
            StagedFile {
                name: "file".to_owned(),
                size: 5,
                content_type: None,
            },
        ]
    );
    assert!(batch.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    assert_eq!(files, &[picture.to_vec(), b"notes".to_vec()]);
    drop(core);
    assert_eq!(
        staged(&fixture)?,
        0,
        "the staged files outlived core's answer"
    );
    Ok(())
}

#[tokio::test]
async fn a_retry_with_the_same_event_id_is_the_first_message_again() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let first = send(&fixture, &token, "?agent=assistant", &[Part::Text("hello")]).await?;
    assert_eq!(first.json()?["duplicate"], false);
    fixture
        .core()
        .conversations
        .push(conversation("assistant", CHAT, 1));
    let again = send(&fixture, &token, "?agent=assistant", &[Part::Text("hello")]).await?;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(
        again.json()?,
        json!({"turn_id": format!("turn-{EVENT}"), "duplicate": true})
    );
    // A plain message says nothing of intent.
    assert_eq!(fixture.core().events[0].0.native, json!({}));
    Ok(())
}

#[tokio::test]
async fn a_conversation_keeps_the_agent_of_its_first_message() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let hello = [Part::Text("hello")];
    for (query, status, said) in [
        ("", StatusCode::UNPROCESSABLE_ENTITY, "names its agent"),
        (
            "?agent=nobody",
            StatusCode::UNPROCESSABLE_ENTITY,
            "no agent nobody",
        ),
    ] {
        let answer = send(&fixture, &token, query, &hello).await?;
        assert_eq!(answer.status, status, "{query}");
        assert!(answer.text().contains(said), "{}", answer.text());
    }
    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 1));
    let other = send(&fixture, &token, "?agent=assistant", &hello).await?;
    assert_eq!(other.status, StatusCode::CONFLICT);
    assert!(other.text().contains("is with second"), "{}", other.text());
    assert!(
        fixture.core().events.is_empty(),
        "a refused message reached core"
    );

    for query in ["", "?agent=second"] {
        let answer = send(&fixture, &token, query, &hello).await?;
        assert_eq!(answer.status, StatusCode::OK, "{query}");
    }
    let core = fixture.core();
    assert!(
        core.events
            .iter()
            .all(|(event, _)| event.agent_id == "second")
    );
    assert_eq!(core.events.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_message_is_named_only_as_the_page_mints_it() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    for event in [
        "0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
        "m-0190F3A8-7B1C-7DEF-8A2B-AAAAAAAAAAAA",
        "m-0190f3a87b1c7def8a2baaaaaaaaaaaa",
        "m-",
        "runtime:0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
        "M-0190f3a8-7b1c-7def-8a2b-aaaaaaaaaaaa",
    ] {
        let body = form(&[Part::Text("hello")]);
        let answer = fixture
            .send(message(&token, &path(event, "?agent=assistant"), body)?)
            .await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{event}");
    }
    let core = fixture.core();
    assert!(core.events.is_empty() && core.asked.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_message_that_cannot_be_taken_is_answered_in_words_and_leaves_nothing_staged()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let query = "?agent=assistant";
    let big = vec![b'a'; MIB + 1];
    let most = vec![b'b'; MIB - 1];
    let text = "t".repeat(MIB + 1);
    for (parts, status, said) in [
        (
            vec![
                Part::Text("four"),
                small_file(),
                small_file(),
                small_file(),
                small_file(),
            ],
            StatusCode::PAYLOAD_TOO_LARGE,
            "at most 3 files",
        ),
        (
            vec![
                Part::File("big.bin", None, &big),
                Part::Text("after the file"),
            ],
            StatusCode::PAYLOAD_TOO_LARGE,
            "at most 1 MiB",
        ),
        (
            vec![
                Part::File("one.bin", None, &most),
                Part::File("two.bin", None, &most),
                Part::File("three.bin", None, &most),
            ],
            StatusCode::PAYLOAD_TOO_LARGE,
            "2 MiB together",
        ),
        (
            vec![Part::Text(&text)],
            StatusCode::PAYLOAD_TOO_LARGE,
            "text may have at most",
        ),
        (
            vec![Part::Text("  \n")],
            StatusCode::BAD_REQUEST,
            "needs text or a file",
        ),
        (vec![], StatusCode::BAD_REQUEST, "needs text or a file"),
    ] {
        let answer = send(&fixture, &token, query, &parts).await?;
        assert_eq!(answer.status, status, "{said}: {}", answer.text());
        let error = answer.json()?["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains(said), "{error}");
        assert_eq!(staged(&fixture)?, 0, "{said}");
    }
    assert!(
        fixture.core().events.is_empty(),
        "a refused message reached core"
    );
    Ok(())
}

#[tokio::test]
async fn a_malformed_message_and_a_refusal_of_core_are_answered_in_words_too() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let query = "?agent=assistant";
    let whole = form(&[
        Part::File("a.txt", None, b"some bytes"),
        Part::Text("hello"),
    ]);
    let cut = whole[..whole.len() - 40].to_vec();
    let unknown = form(&[Part::Text("hello")])
        .into_iter()
        .collect::<Vec<u8>>();
    let unknown = String::from_utf8(unknown)?.replace("name=\"text\"", "name=\"native\"");
    for (request, said) in [
        (
            message(&token, &path(EVENT, query), cut)?,
            "did not arrive whole",
        ),
        (
            message(&token, &path(EVENT, query), b"not a form at all".to_vec())?,
            "did not arrive whole",
        ),
        (
            message(&token, &path(EVENT, query), unknown.into_bytes())?,
            "parts are `text` and `file`",
        ),
        (
            bearer(Method::PUT, &path(EVENT, query), &token)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from("{\"text\":\"hello\"}"))?,
            "multipart form data",
        ),
    ] {
        let answer = fixture.send(request).await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{said}");
        let error = answer.json()?["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(error.contains(said), "{error}");
        assert_eq!(staged(&fixture)?, 0, "{said}");
    }
    assert!(
        fixture.core().events.is_empty(),
        "a refused message reached core"
    );

    // Core's own refusal reaches the page in core's words.
    fixture.core().refusal = Some((
        StatusCode::INSUFFICIENT_STORAGE,
        "attachment storage is full",
    ));
    let full = send(&fixture, &token, query, &[Part::File("a.txt", None, b"a")]).await?;
    assert_eq!(full.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(full.json()?, json!({"error": "attachment storage is full"}));
    assert_eq!(staged(&fixture)?, 0);

    fixture.core().refusal = None;
    fixture.web.stopping.cancel();
    let stopping = send(&fixture, &token, query, &[Part::File("a.txt", None, b"a")]).await?;
    assert_eq!(stopping.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(stopping.text().contains("Send the message again"));
    assert_eq!(staged(&fixture)?, 0);
    Ok(())
}

fn small_file() -> Part<'static> {
    Part::File("a.txt", Some("text/plain"), b"a")
}

#[tokio::test]
async fn a_browser_that_leaves_after_its_upload_does_not_take_the_files_from_core() -> Result<()> {
    let fixture = Arc::new(Fixture::new().await?);
    let token = fixture.sign_in().await?;
    let hold = Arc::new(Notify::new());
    fixture.core().hold = Some(Arc::clone(&hold));
    let request = message(
        &token,
        &path(EVENT, "?agent=assistant"),
        form(&[Part::File("kept.txt", None, b"still here")]),
    )?;
    let browser = tokio::spawn({
        let fixture = Arc::clone(&fixture);
        async move { fixture.send(request).await.map(|answer| answer.status) }
    });
    let reached = |fixture: &Fixture| {
        fixture
            .core()
            .asked
            .iter()
            .any(|asked| asked == "/v1/events")
    };
    while !reached(&fixture) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    browser.abort();
    assert!(browser.await.is_err_and(|ended| ended.is_cancelled()));
    assert_eq!(staged(&fixture)?, 1, "the files left with the browser");
    hold.notify_one();
    while fixture.core().events.is_empty() || staged(&fixture)? > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(fixture.core().events[0].1, [b"still here".to_vec()]);
    Ok(())
}
