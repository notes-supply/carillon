//! An answer's files on Discord: read from core's copies and uploaded with
//! the answer's text, within Discord's limits.

use std::{fs, path::Path as FilePath};

use axum::{body::Bytes, http::HeaderMap};
use carillon_installation::config::MIB;
use carillon_protocol::model::AnswerFile;
use sha2::{Digest as _, Sha256};

use super::{super::files::compose, *};

/// A message Discord received: its text and each file's name and bytes.
#[derive(Debug, PartialEq, Eq)]
struct Received {
    content: String,
    files: Vec<(String, Vec<u8>)>,
}

/// Decodes a message request: JSON, or the multipart form twilight sends
/// when a message carries files.
fn received(headers: &HeaderMap, body: &[u8]) -> anyhow::Result<Received> {
    let kind = headers
        .get("content-type")
        .context("no content type")?
        .to_str()?;
    let Some((_, boundary)) = kind.split_once("boundary=") else {
        let payload: serde_json::Value = serde_json::from_slice(body)?;
        return Ok(Received {
            content: payload["content"].as_str().unwrap_or_default().to_owned(),
            files: Vec::new(),
        });
    };
    let mut content = String::new();
    let mut files = Vec::new();
    for part in split(body, format!("--{boundary}").as_bytes()) {
        let Some(part) = part.strip_prefix(b"\r\n") else {
            continue;
        };
        let at = part
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .context("a part without headers")?;
        let headers = String::from_utf8_lossy(&part[..at]);
        let value = part[at + 4..]
            .strip_suffix(b"\r\n")
            .context("a part without its end")?;
        if let Some((_, filename)) = headers.split_once("filename=\"") {
            let filename = filename.split('"').next().unwrap_or_default();
            files.push((filename.to_owned(), value.to_vec()));
        } else {
            let payload: serde_json::Value = serde_json::from_slice(value)?;
            content = payload["content"].as_str().unwrap_or_default().to_owned();
        }
    }
    Ok(Received { content, files })
}

fn split<'a>(body: &'a [u8], separator: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut rest = body;
    while let Some(at) = rest
        .windows(separator.len())
        .position(|window| window == separator)
    {
        parts.push(&rest[..at]);
        rest = &rest[at + separator.len()..];
    }
    parts.push(rest);
    parts
}

/// Core handing out `claims` in order, a transient failure where one is
/// `None`, then refusing to end the loop; beside Discord, which records
/// each message it receives.
fn uploading_core(
    claims: Vec<Option<Delivery>>,
    messages: Arc<tokio::sync::Mutex<Vec<Received>>>,
    acks: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
) -> Router {
    let claims = Arc::new(std::sync::Mutex::new(claims.into_iter()));
    Router::new()
        .route(
            "/v1/deliveries",
            get(move || {
                let next = claims.lock().ok().and_then(|mut claims| claims.next());
                async move {
                    match next {
                        Some(Some(delivery)) => Json(Some(delivery)).into_response(),
                        Some(None) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        None => StatusCode::FORBIDDEN.into_response(),
                    }
                }
            }),
        )
        .route(
            "/v1/deliveries/reconcile",
            post(|| async {
                Json(DeliveryReconciliation {
                    feedback_target_ids: Vec::new(),
                })
            }),
        )
        .route(
            "/v1/deliveries/{id}/ack",
            post(move |Json(ack): Json<serde_json::Value>| async move {
                acks.lock().await.push(ack);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post(move |headers: HeaderMap, body: Bytes| async move {
                let Ok(message) = received(&headers, &body) else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                let mut messages = messages.lock().await;
                let body = json!({"content": message.content});
                messages.push(message);
                Json(created(900 + messages.len(), &body)).into_response()
            }),
        )
        .fallback(|| async { StatusCode::NO_CONTENT })
}

/// Runs the delivery loop against `claims` with core's copies in `store`:
/// the messages Discord received and the acknowledgements core received.
async fn upload(
    claims: Vec<Option<Delivery>>,
    store: &FilePath,
) -> anyhow::Result<(Vec<Received>, Vec<serde_json::Value>)> {
    let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let acks = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = uploading_core(claims, messages.clone(), acks.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let ended = tokio::time::timeout(
        Duration::from_secs(10),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            Reactions::new(http.clone(), crate::config::Settings::new(config)),
            http,
            store.to_path_buf(),
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await?;
    server.abort();
    assert!(
        ended
            .err()
            .context("the delivery loop ended without its claims refused")?
            .to_string()
            .contains("delivery claim rejected")
    );
    Ok((
        std::mem::take(&mut *messages.lock().await),
        std::mem::take(&mut *acks.lock().await),
    ))
}

/// Keeps `bytes` in `store` as core does, as file `ordinal` of a capture.
fn kept(store: &FilePath, ordinal: u32, name: &str, bytes: &[u8]) -> anyhow::Result<AnswerFile> {
    let path = format!("0190a6e0-0000-7000-8000-000000000001/{ordinal}");
    fs::create_dir_all(store.join(&path).parent().context("no capture")?)?;
    fs::write(store.join(&path), bytes)?;
    Ok(AnswerFile {
        path,
        name: name.to_owned(),
        source: format!("/home/owner/out/{name}"),
        size: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(bytes)),
    })
}

fn listed(name: &str, size: u64) -> AnswerFile {
    AnswerFile {
        path: format!("0190a6e0-0000-7000-8000-000000000002/{size}"),
        name: name.to_owned(),
        source: format!("/home/owner/out/{name}"),
        size,
        sha256: "0".repeat(64),
    }
}

#[tokio::test]
async fn an_answers_files_reach_discord_with_its_text_once_a_lost_claim_is_taken_up()
-> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let delivery = Delivery {
        content: "Here is the chart.".to_owned(),
        files: vec![
            kept(store.path(), 0, "chart.png", b"\x89PNG chart")?,
            kept(store.path(), 1, "data set (2).csv", b"a,b\n1,2\n")?,
        ],
        ..answer(1, TurnState::Completed)
    };
    // The first claim's answer is lost: core may hold it for this provider,
    // which takes it up again from core's copies.
    let (messages, acks) = upload(vec![None, Some(delivery)], store.path()).await?;
    assert_eq!(
        messages,
        [Received {
            content: "Here is the chart.".to_owned(),
            files: vec![
                ("chart.png".to_owned(), b"\x89PNG chart".to_vec()),
                ("data_set__2_.csv".to_owned(), b"a,b\n1,2\n".to_vec()),
            ],
        }]
    );
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0]["outcome"], "delivered");
    assert_eq!(acks[0]["native_ref"], "901");
    Ok(())
}

#[tokio::test]
async fn a_file_over_discords_limit_is_named_in_the_answer_and_the_rest_sent() -> anyhow::Result<()>
{
    let store = tempfile::tempdir()?;
    let delivery = Delivery {
        content: "Here they are.".to_owned(),
        files: vec![
            listed("talk.mp4", 21 * MIB),
            kept(store.path(), 1, "notes.txt", b"notes")?,
        ],
        ..answer(1, TurnState::Completed)
    };
    let (messages, acks) = upload(vec![Some(delivery)], store.path()).await?;
    assert_eq!(
        messages,
        [Received {
            content: "Here they are.\n\nCould not send talk.mp4: it has 21 MiB, over the 20 MiB \
                      Discord takes of one file. It is at /home/owner/out/talk.mp4."
                .to_owned(),
            files: vec![("notes.txt".to_owned(), b"notes".to_vec())],
        }]
    );
    assert_eq!(acks[0]["outcome"], "delivered");
    Ok(())
}

#[tokio::test]
async fn a_copy_that_changed_after_it_was_kept_is_never_sent() -> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let chart = kept(store.path(), 0, "chart.png", b"chart")?;
    fs::write(store.path().join(&chart.path), b"altered")?;
    let delivery = Delivery {
        files: vec![chart],
        ..answer(1, TurnState::Completed)
    };
    let (messages, acks) = upload(vec![Some(delivery)], store.path()).await?;
    assert!(messages.is_empty(), "{messages:?}");
    assert_eq!(acks[0]["outcome"], "uncertain");
    Ok(())
}

#[test]
fn files_fill_each_message_as_far_as_discord_takes_them() {
    let names = |files: &[&AnswerFile]| -> Vec<String> {
        files.iter().map(|file| file.name.clone()).collect()
    };
    // Ten files to a message.
    let small = (0..12)
        .map(|index| listed(&format!("{index}.txt"), 1))
        .collect::<Vec<_>>();
    let messages = compose("Here.", &small);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "Here.");
    assert_eq!(names(&messages[0].files).len(), 10);
    assert_eq!(names(&messages[1].files), ["10.txt", "11.txt"]);
    assert!(messages[1].content.is_empty());
    // Within 25 MiB a message, each file whole.
    let large = (0..4)
        .map(|index| listed(&format!("{index}.bin"), 10 * MIB))
        .collect::<Vec<_>>();
    let messages = compose("Here.", &large);
    let sizes = messages
        .iter()
        .map(|message| message.files.iter().map(|file| file.size).sum::<u64>())
        .collect::<Vec<_>>();
    assert_eq!(sizes, [20 * MIB, 20 * MIB]);
    // A file of exactly Discord's limit is sent; the text's last message
    // carries the first files.
    let long = "a ".repeat(1_500);
    let edge = [listed("edge.bin", 20 * MIB)];
    let messages = compose(&long, &edge);
    assert_eq!(messages.len(), 2);
    assert!(messages[0].files.is_empty());
    assert_eq!(names(&messages[1].files), ["edge.bin"]);
}
