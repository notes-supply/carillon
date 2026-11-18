//! Sending: the identities a message gets, what a retry does, and what the
//! page is told when a message cannot be taken.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
};
use carillon_protocol::model::{ExternalEvent, StagedAttachments, StagedFile};
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use serde_json::json;
use tokio::{io::AsyncWriteExt as _, sync::Notify};
use tokio_util::io::ReaderStream;

use super::bound::REFUSED_DRAIN;
use crate::{
    api::AWAY,
    fixture::{Answer, Fixture, bearer},
};

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

fn path(agent: &str, event: &str) -> String {
    format!("/api/conversations/{agent}/{CHAT}/messages/{event}")
}

async fn send(fixture: &Fixture, token: &str, agent: &str, parts: &[Part<'_>]) -> Result<Answer> {
    fixture
        .send(message(token, &path(agent, EVENT), form(parts))?)
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
        "second",
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
        json!({"turn_id": "turn-1", "duplicate": false})
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
    assert_eq!(event.native, json!({}));
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
            StagedFile {
                name: "file".to_owned(),
                size: 5,
                content_type: None,
            },
        ]
    );
    assert!(!batch.is_empty());
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
async fn a_file_keeps_the_name_the_person_s_file_has() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    // As a browser writes `say "hi"`, a name with a line end, and a name that has a percent sign.
    let written = [
        "say %22hi%22.txt",
        "two%0D%0Alines.txt",
        "100%25 %41 %2F.txt",
    ];
    let parts: Vec<_> = std::iter::once(Part::Text("three files"))
        .chain(written.map(|name| Part::File(name, None, b"x")))
        .collect();
    let sent = send(&fixture, &token, "second", &parts).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    let core = fixture.core();
    let [(event, _)] = core.events.as_slice() else {
        anyhow::bail!("expected one event, not {}", core.events.len());
    };
    let named: Vec<_> = event
        .attachments
        .iter()
        .flat_map(|attached| &attached.files)
        .map(|file| file.name.as_str())
        .collect();
    assert_eq!(
        named,
        ["say \"hi\".txt", "two\r\nlines.txt", "100%25 %41 %2F.txt"]
    );
    Ok(())
}

#[tokio::test]
async fn a_retry_with_the_same_event_id_is_the_first_message_again() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let first = send(&fixture, &token, "assistant", &[Part::Text("hello")]).await?;
    let first = first.json()?;
    assert_eq!(first["duplicate"], false);
    let again = send(&fixture, &token, "assistant", &[Part::Text("hello")]).await?;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(
        again.json()?,
        json!({"turn_id": first["turn_id"], "duplicate": true})
    );
    let core = fixture.core();
    let [(event, _)] = core.events.as_slice() else {
        anyhow::bail!("core admitted one event ID {} times", core.events.len());
    };
    assert_eq!(event.native, json!({}));
    Ok(())
}

/// What the provider told core of each message it submitted: whether its
/// text invokes a command.
fn intents(fixture: &Fixture) -> Vec<(String, bool)> {
    let core = fixture.core();
    let intent = |event: &ExternalEvent| event.native == json!({"intent": "command"});
    core.events
        .iter()
        .map(|(event, _)| (event.content.clone(), intent(event)))
        .collect()
}

#[tokio::test]
async fn a_text_that_begins_with_one_of_the_runtime_s_commands_is_sent_as_a_command() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let texts = [
        ("/skill:assistant", true),
        ("/skill:assistant with its arguments", true),
        ("/skill:assistant\non the next line", true),
        ("/skill:assistantly", false),
        ("/skill:second", false),
        ("/skill", false),
        (" /skill:assistant", false),
        ("run /skill:assistant", false),
        ("/", false),
    ];
    for (ordinal, (text, _)) in texts.iter().enumerate() {
        let event = format!("m-0190f3a8-7b1c-7def-8a2b-{ordinal:012}");
        let request = message(
            &token,
            &path("assistant", &event),
            form(&[Part::Text(text)]),
        )?;
        let sent = fixture.send(request).await?;
        assert_eq!(sent.status, StatusCode::OK, "{text}: {}", sent.text());
    }
    let expected: Vec<_> = texts
        .iter()
        .map(|(text, command)| ((*text).to_owned(), *command))
        .collect();
    assert_eq!(intents(&fixture), expected);

    let view = format!("conversations=1&agent_id=assistant&audience_id={CHAT}&turns=50");
    let snapshot = fixture.stream(&view, &token).await?.snapshot().await?;
    let shown: Vec<_> = snapshot["open"]["turns"]
        .as_array()
        .context("no turns")?
        .iter()
        .map(|turn| turn["source"]["command"].as_bool())
        .collect();
    let decided: Vec<_> = texts.iter().map(|(_, command)| Some(*command)).collect();
    assert_eq!(shown, decided);
    Ok(())
}

#[tokio::test]
async fn a_text_that_begins_with_no_slash_asks_no_runtime_for_its_commands() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let sent = send(&fixture, &token, "assistant", &[Part::Text("hello")]).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(intents(&fixture), [("hello".to_owned(), false)]);
    assert_eq!(fixture.core().asked, ["/v1/events"]);
    Ok(())
}

#[tokio::test]
async fn a_runtime_that_cannot_say_its_commands_leaves_the_text_a_message_and_is_asked_once()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    fixture.core().away_for_requests = 1;
    let text = "/skill:assistant now";
    let sent = send(&fixture, &token, "assistant", &[Part::Text(text)]).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(intents(&fixture), [(text.to_owned(), false)]);
    let commands = format!("/v1/commands?agent_id=assistant&channel_id={CHAT}&audience_id={CHAT}");
    assert_eq!(fixture.core().asked, [commands.as_str(), "/v1/events"]);
    Ok(())
}

#[tokio::test]
async fn a_message_for_an_agent_the_installation_lacks_is_read_whole_and_then_refused() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let rest = vec![b'r'; 64 * 1024];
    let body = form(&[Part::Text("hello"), Part::File("rest.bin", None, &rest)]);
    let (mut browser, provider) = tokio::io::duplex(1024);
    let upload = tokio::spawn(async move { browser.write_all(&body).await });
    let request = bearer(Method::PUT, &path("nobody", EVENT), &token)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from_stream(ReaderStream::new(provider)))?;
    let answer = fixture.send(request).await?;
    upload
        .await?
        .context("answered while the browser was uploading")?;
    assert_eq!(answer.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        answer.json()?,
        json!({"error": "Carillon has no agent nobody."})
    );
    assert_eq!(staged(&fixture)?, 0);
    assert!(fixture.core().asked.is_empty(), "core was asked");
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
            .send(message(&token, &path("assistant", event), body)?)
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
    let agent = "assistant";
    let big = vec![b'a'; MIB + 1];
    let most = vec![b'b'; MIB - 1];
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
            vec![Part::Text("  \n")],
            StatusCode::BAD_REQUEST,
            "needs text or a file",
        ),
        (vec![], StatusCode::BAD_REQUEST, "needs text or a file"),
    ] {
        let answer = send(&fixture, &token, agent, &parts).await?;
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
    let agent = "assistant";
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
            message(&token, &path(agent, EVENT), cut)?,
            "did not arrive whole",
        ),
        (
            message(&token, &path(agent, EVENT), b"not a form at all".to_vec())?,
            "did not arrive whole",
        ),
        (
            message(&token, &path(agent, EVENT), unknown.into_bytes())?,
            "parts are `text` and `file`",
        ),
        (
            bearer(Method::PUT, &path(agent, EVENT), &token)
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

    fixture.core().refusal = Some((
        StatusCode::INSUFFICIENT_STORAGE,
        "attachment storage is full",
    ));
    let full = send(&fixture, &token, agent, &[Part::File("a.txt", None, b"a")]).await?;
    assert_eq!(full.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(full.json()?, json!({"error": "attachment storage is full"}));
    assert_eq!(staged(&fixture)?, 0);

    fixture.core().refusal = None;
    fixture.web.stopping.cancel();
    let stopping = send(&fixture, &token, agent, &[Part::File("a.txt", None, b"a")]).await?;
    assert_eq!(stopping.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stopping.json()?, json!({ "error": AWAY }));
    assert_eq!(staged(&fixture)?, 0);
    Ok(())
}

#[tokio::test]
async fn a_send_waits_out_a_core_that_is_away_for_a_moment() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let hello = [Part::Text("hello")];
    fixture.core().away_for_requests = 2;
    let sent = send(&fixture, &token, "assistant", &hello).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(fixture.core().events.len(), 1);

    fixture.core().away_for_requests = usize::MAX;
    fixture.web.stopping.cancel();
    let stopping = send(&fixture, &token, "assistant", &hello).await?;
    assert_eq!(stopping.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stopping.json()?, json!({ "error": AWAY }));
    assert_eq!(fixture.core().events.len(), 1);
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
        &path("assistant", EVENT),
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while !reached(&fixture) {
        assert!(Instant::now() < deadline, "the message never reached core");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    browser.abort();
    assert!(browser.await.is_err_and(|ended| ended.is_cancelled()));
    assert_eq!(staged(&fixture)?, 1, "the files left with the browser");
    hold.notify_one();
    while fixture.core().events.is_empty() || staged(&fixture)? > 0 {
        assert!(Instant::now() < deadline, "core never took the files");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(fixture.core().events[0].1, [b"still here".to_vec()]);
    Ok(())
}

fn submitted(fixture: &Fixture) -> bool {
    fixture
        .core()
        .asked
        .iter()
        .any(|asked| asked == "/v1/events")
}

#[tokio::test]
async fn a_message_s_text_is_bounded_by_the_event_core_reads() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let agent = "assistant";
    let over = "a".repeat(2 * MIB + 1);
    // Each quote is two bytes of the event's JSON.
    let over_as_an_event = "\"".repeat(MIB);
    for (text, why) in [
        (&over, "longer than an event"),
        (
            &over_as_an_event,
            "longer than an event once written as one",
        ),
    ] {
        let answer = send(&fixture, &token, agent, &[Part::Text(text)]).await?;
        assert!(
            !submitted(&fixture),
            "{why}: core was sent text it does not read"
        );
        assert_eq!(
            answer.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{why}: {}",
            answer.text()
        );
        assert_eq!(
            answer.json()?,
            json!({"error": "A message takes about 2 MiB of text at most."}),
            "{why}"
        );
    }

    let long = "a".repeat(MIB);
    let sent = send(&fixture, &token, agent, &[Part::Text(&long)]).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(fixture.core().events[0].0.content, long);
    Ok(())
}

#[tokio::test]
async fn a_leading_byte_order_mark_is_not_part_of_the_text() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let text = "\u{feff}hello \u{feff}there";
    let sent = send(&fixture, &token, "assistant", &[Part::Text(text)]).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(fixture.core().events[0].0.content, "hello \u{feff}there");
    Ok(())
}

#[tokio::test]
async fn a_message_has_one_text_part() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    for parts in [
        vec![Part::Text("first"), Part::Text("second")],
        vec![Part::Text("first"), small_file(), Part::Text("")],
    ] {
        let answer = send(&fixture, &token, "assistant", &parts).await?;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{} parts: {}",
            parts.len(),
            answer.text()
        );
        assert_eq!(
            answer.json()?,
            json!({"error": "A message has one `text` part."})
        );
        assert_eq!(staged(&fixture)?, 0);
    }
    assert!(!submitted(&fixture), "a refused message reached core");
    Ok(())
}

#[tokio::test]
async fn a_refused_message_is_read_to_its_end_before_it_is_answered() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let over = "a".repeat(3 * MIB);
    let rest = vec![b'r'; 64 * 1024];
    let rest = || Part::File("rest.bin", None, &rest);
    let unknown = String::from_utf8(form(&[Part::Text("hello"), rest()]))?
        .replacen("name=\"text\"", "name=\"native\"", 1)
        .into_bytes();
    for (body, said) in [
        (unknown, "parts are `text` and `file`"),
        (
            form(&[Part::Text("first"), Part::Text("second"), rest()]),
            "one `text` part",
        ),
        (form(&[Part::Text(&over), rest()]), "2 MiB of text"),
    ] {
        let (mut browser, provider) = tokio::io::duplex(1024);
        let upload = tokio::spawn(async move { browser.write_all(&body).await });
        let request = bearer(Method::PUT, &path("assistant", EVENT), &token)
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from_stream(ReaderStream::new(provider)))?;
        let answer = fixture.send(request).await?;
        upload
            .await?
            .with_context(|| format!("{said}: answered while the browser was uploading"))?;
        assert!(answer.text().contains(said), "{}", answer.text());
        assert_eq!(staged(&fixture)?, 0, "{said}");
    }
    assert!(!submitted(&fixture), "a refused message reached core");
    Ok(())
}

#[tokio::test]
async fn bytes_between_parts_past_the_bound_are_refused_without_holding_them_whole() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let flood = "A".repeat(256 * 1024);
    let text =
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"text\"\r\n\r\nhello\r\n");
    // None of these reaches the end the parser looks for: a boundary, a
    // header's blank line, or the line end after a boundary. Nothing but the
    // bound stops the parser reading all of it.
    for (body, said) in [
        (flood.clone(), "a preamble"),
        (
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{flood}"
            ),
            "a first part's header",
        ),
        (
            format!("{text}--{BOUNDARY}\r\nX-Pad: {flood}"),
            "a header after a part's data",
        ),
        (
            format!("{text}--{BOUNDARY}{}", " ".repeat(flood.len())),
            "padding after a boundary",
        ),
    ] {
        let (mut browser, provider) = tokio::io::duplex(64 * 1024);
        let upload = tokio::spawn(async move { browser.write_all(body.as_bytes()).await });
        let request = bearer(Method::PUT, &path("assistant", EVENT), &token)
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from_stream(ReaderStream::new(provider)))?;
        let answer = fixture.send(request).await?;
        upload.abort();
        assert_eq!(
            answer.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{said}: {}",
            answer.text()
        );
        assert_eq!(
            answer.json()?,
            json!({"error": "A message's parts are larger than the web chat reads."}),
            "{said}"
        );
        assert_eq!(staged(&fixture)?, 0, "{said}");
    }
    assert!(fixture.core().asked.is_empty(), "core was asked");
    Ok(())
}

#[tokio::test]
async fn a_refused_message_is_drained_no_further_than_2_mib() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    // An unknown first part refuses the message. The rest carries four drains'
    // worth, in one large part or in many empty parts whose headers each stay
    // within the between-parts bound.
    let mut large = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"big.bin\"\r\n\r\n"
    )
    .into_bytes();
    large.resize(large.len() + REFUSED_DRAIN * 4, b'r');
    large.extend_from_slice(b"\r\n");
    let empty = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"native\"\r\nX-Pad: {}\r\n\r\n\r\n",
        "p".repeat(8000)
    );
    let empties = empty.repeat(REFUSED_DRAIN * 4 / empty.len()).into_bytes();
    for (rest, said) in [(large, "one large part"), (empties, "many empty parts")] {
        let mut body = format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"native\"\r\n\r\nhi\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&rest);
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        // Frames of up to 128 KiB, as large as hyper reads them.
        let read = Arc::new(AtomicUsize::new(0));
        let (mut browser, provider) = tokio::io::duplex(128 * 1024);
        let upload = tokio::spawn(async move { browser.write_all(&body).await });
        let counted = {
            let read = Arc::clone(&read);
            ReaderStream::with_capacity(provider, 128 * 1024).inspect_ok(move |chunk| {
                read.fetch_add(chunk.len(), Relaxed);
            })
        };
        let request = bearer(Method::PUT, &path("assistant", EVENT), &token)
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from_stream(counted))?;
        let answer = fixture.send(request).await?;
        let _ = upload.await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{said}");
        assert_eq!(
            answer.json()?,
            json!({"error": "A message's parts are `text` and `file`."}),
            "{said}"
        );
        // Besides the drain, the provider may have fed the parser the first
        // part's whole 64 KiB window before it refused the message, and read
        // one frame it has not fed.
        assert!(
            read.load(Relaxed) <= 2 * MIB + 64 * 1024 + 128 * 1024,
            "{said}: the provider read {} bytes of a refused message",
            read.load(Relaxed)
        );
    }
    assert!(
        fixture.core().events.is_empty(),
        "a refused message reached core"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn an_upload_that_pauses_29_seconds_between_pieces_is_taken_whole() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let file = vec![b'f'; 8192];
    let body = form(&[Part::Text("slow"), Part::File("slow.bin", None, &file)]);
    let pieces: Vec<_> = body
        .chunks(body.len().div_ceil(3))
        .map(<[u8]>::to_vec)
        .collect();
    let slow = stream::iter(pieces).then(|piece| async move {
        tokio::time::sleep(Duration::from_secs(29)).await;
        Ok::<_, std::io::Error>(piece)
    });
    let start = tokio::time::Instant::now();
    let request = bearer(Method::PUT, &path("assistant", EVENT), &token)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from_stream(slow))?;
    let sent = fixture.send(request).await?;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(start.elapsed(), Duration::from_secs(3 * 29));
    let core = fixture.core();
    let [(event, _)] = core.events.as_slice() else {
        anyhow::bail!("expected one event, not {}", core.events.len());
    };
    let sizes: Vec<_> = event
        .attachments
        .iter()
        .flat_map(|attached| &attached.files)
        .map(|file| file.size)
        .collect();
    assert_eq!(sizes, [8192]);
    Ok(())
}
