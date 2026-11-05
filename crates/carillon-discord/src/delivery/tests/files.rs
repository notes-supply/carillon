//! An answer's files on Discord: read from core's copies and uploaded with
//! the answer's text, within Discord's limits.

use std::{fs, path::Path as FilePath};

use axum::{body::Bytes, http::HeaderMap, routing::put};
use carillon_installation::config::{MIB, Provider, ProviderRole};
use carillon_protocol::model::AnswerFile;
use sha2::{Digest as _, Sha256};

use super::{super::files::compose, *};
use crate::delivery::require_upload_limit;

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

/// What the delivery loop sent: the messages Discord received, the
/// reactions it was asked to add, and the acknowledgements core received.
#[derive(Default)]
struct Uploaded {
    messages: Vec<Received>,
    reactions: Vec<String>,
    acks: Vec<serde_json::Value>,
}

/// Core handing out `claims` in order, a transient failure where one is
/// `None`, then refusing to end the loop; beside Discord, which records
/// what it receives and fails the message numbered `refused`, if any.
fn uploading_core(
    claims: Vec<Option<Delivery>>,
    refused: Option<usize>,
    uploaded: Arc<tokio::sync::Mutex<Uploaded>>,
) -> Router {
    let claims = Arc::new(std::sync::Mutex::new(claims.into_iter()));
    let (acked, sent, reacted) = (uploaded.clone(), uploaded.clone(), uploaded);
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
                acked.lock().await.acks.push(ack);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages",
            post(move |headers: HeaderMap, body: Bytes| async move {
                let Ok(message) = received(&headers, &body) else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                let messages = &mut sent.lock().await.messages;
                let body = json!({"content": message.content});
                messages.push(message);
                if refused == Some(messages.len()) {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                Json(created(900 + messages.len(), &body)).into_response()
            }),
        )
        .route(
            "/api/v10/channels/{channel}/messages/{message}/reactions/{emoji}/@me",
            put(
                move |Path((channel, message, emoji)): Path<(String, String, String)>| async move {
                    reacted
                        .lock()
                        .await
                        .reactions
                        .push(format!("{channel}:{message} {emoji}"));
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .fallback(|| async { StatusCode::NO_CONTENT })
}

/// Runs the delivery loop against `claims` with core's copies in `store`,
/// Discord failing the message numbered `refused`, until its reactions end.
async fn upload_refusing(
    claims: Vec<Option<Delivery>>,
    store: &FilePath,
    refused: Option<usize>,
) -> anyhow::Result<Uploaded> {
    let uploaded = Arc::new(tokio::sync::Mutex::new(Uploaded::default()));
    let app = uploading_core(claims, refused, uploaded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let http = discord_client(address);
    let config = discord_config(DiscordActivation::Always, DiscordConversation::Channel);
    let reactions = Reactions::new(http.clone(), crate::config::Settings::new(config));
    let (outcomes, _) = tokio::sync::mpsc::unbounded_channel();
    let records = tempfile::tempdir()?;
    let ended = tokio::time::timeout(
        Duration::from_secs(10),
        delivery_loop(
            core_client(format!("http://{address}"))?,
            reactions.clone(),
            participation(records.path())?,
            store.to_path_buf(),
            outcomes,
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await?;
    reactions.stop(Duration::from_secs(5)).await;
    server.abort();
    assert!(
        ended
            .err()
            .context("the delivery loop ended without its claims refused")?
            .to_string()
            .contains("delivery claim rejected")
    );
    Ok(std::mem::take(&mut *uploaded.lock().await))
}

/// As [`upload_refusing`], with Discord taking every message.
async fn upload(claims: Vec<Option<Delivery>>, store: &FilePath) -> anyhow::Result<Uploaded> {
    upload_refusing(claims, store, None).await
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
    let Uploaded { messages, acks, .. } = upload(vec![None, Some(delivery)], store.path()).await?;
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
async fn a_copy_that_changed_after_it_was_kept_is_never_sent() -> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let chart = kept(store.path(), 0, "chart.png", b"chart")?;
    fs::write(store.path().join(&chart.path), b"altered")?;
    let delivery = Delivery {
        files: vec![chart],
        ..answer(1, TurnState::Completed)
    };
    let Uploaded { messages, acks, .. } = upload(vec![Some(delivery)], store.path()).await?;
    assert!(messages.is_empty(), "{messages:?}");
    assert_eq!(acks[0]["outcome"], "uncertain");
    Ok(())
}

/// Twelve small files, kept in `store`: ten on the answer's message, two on
/// one after it.
fn overflowing(store: &FilePath) -> anyhow::Result<Delivery> {
    Ok(Delivery {
        content: "Here they are.".to_owned(),
        files: (0..12)
            .map(|ordinal| kept(store, ordinal, &format!("{ordinal}.txt"), &[b'x'; 3]))
            .collect::<anyhow::Result<_>>()?,
        ..answer(1, TurnState::Completed)
    })
}

#[tokio::test]
async fn files_past_a_messages_limits_follow_the_answer_once_each() -> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let delivery = overflowing(store.path())?;
    let delivered = discord_config(DiscordActivation::Always, DiscordConversation::Channel)
        .delivered_emoji
        .clone();
    let uploaded = upload(vec![None, Some(delivery)], store.path()).await?;
    let counts = uploaded
        .messages
        .iter()
        .map(|message| (message.content.as_str(), message.files.len()))
        .collect::<Vec<_>>();
    assert_eq!(counts, [("Here they are.", 10), ("", 2)]);
    assert_eq!(uploaded.acks.len(), 1);
    assert_eq!(uploaded.acks[0]["outcome"], "delivered");
    assert_eq!(uploaded.acks[0]["native_ref"], "901,902");
    assert_eq!(uploaded.reactions, [format!("300:400 {delivered}")]);
    Ok(())
}

#[tokio::test]
async fn a_following_message_discord_fails_leaves_the_answer_uncertain_and_sends_nothing_again()
-> anyhow::Result<()> {
    let store = tempfile::tempdir()?;
    let delivery = overflowing(store.path())?;
    let uncertain = discord_config(DiscordActivation::Always, DiscordConversation::Channel)
        .uncertain_emoji
        .clone();
    let uploaded = upload_refusing(vec![Some(delivery)], store.path(), Some(2)).await?;
    let counts = uploaded
        .messages
        .iter()
        .map(|message| message.files.len())
        .collect::<Vec<_>>();
    assert_eq!(counts, [10, 2], "each message is tried once");
    assert_eq!(uploaded.acks.len(), 1);
    assert_eq!(uploaded.acks[0]["outcome"], "uncertain");
    assert_eq!(uploaded.acks[0]["native_ref"], serde_json::Value::Null);
    assert_eq!(uploaded.reactions, [format!("300:400 {uncertain}")]);
    Ok(())
}

fn names(files: &[&AnswerFile]) -> Vec<String> {
    files.iter().map(|file| file.name.clone()).collect()
}

#[test]
fn a_message_carries_at_most_ten_files() {
    let small = (0..12)
        .map(|index| listed(&format!("{index}.txt"), 1))
        .collect::<Vec<_>>();
    let messages = compose("Here.", &small);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content, "Here.");
    assert_eq!(names(&messages[0].files).len(), 10);
    assert!(messages[1].content.is_empty());
    assert_eq!(names(&messages[1].files), ["10.txt", "11.txt"]);
}

#[test]
fn a_message_carries_whole_files_within_its_request_limit() {
    let large = (0..4)
        .map(|index| listed(&format!("{index}.bin"), 10 * MIB))
        .collect::<Vec<_>>();
    let sizes = compose("Here.", &large)
        .iter()
        .map(|message| message.files.iter().map(|file| file.size).sum::<u64>())
        .collect::<Vec<_>>();
    assert_eq!(sizes, [20 * MIB, 20 * MIB]);
}

#[test]
fn a_file_at_discords_limit_goes_on_the_last_message_of_a_long_answer() {
    let edge = [listed("edge.bin", 20 * MIB)];
    let messages = compose(&"a ".repeat(1_500), &edge);
    assert_eq!(messages.len(), 2);
    assert!(messages[0].files.is_empty());
    assert_eq!(names(&messages[1].files), ["edge.bin"]);
}

#[test]
fn discord_takes_an_upload_limit_one_message_carries() -> anyhow::Result<()> {
    let provider = |max_upload_mib| Provider {
        id: "discord-private".to_owned(),
        role: ProviderRole::Interface,
        version: "test".to_owned(),
        url: "http://127.0.0.1:1".to_owned(),
        token_hash: String::new(),
        enabled: true,
        max_upload_mib,
    };
    for accepted in [None, Some(20), Some(24)] {
        require_upload_limit(&provider(accepted))?;
    }
    assert_eq!(provider(None).upload_mib(), 20);
    assert_eq!(
        require_upload_limit(&provider(Some(25)))
            .err()
            .map(|error| error.to_string()),
        Some(
            "max_upload_mib for discord-private is 25, but one Discord message carries at most 24"
                .to_owned()
        )
    );
    Ok(())
}
