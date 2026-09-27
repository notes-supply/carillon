use super::*;
use anyhow::Context as _;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{Method, StatusCode, Uri},
    response::IntoResponse as _,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Step {
    method: Method,
    path: &'static str,
    status: StatusCode,
    body: Value,
}
struct Mock {
    http: Client,
    pending: Arc<Mutex<VecDeque<Step>>>,
    seen: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Mock {
    async fn new(steps: Vec<Step>) -> anyhow::Result<Self> {
        let pending = Arc::new(Mutex::new(VecDeque::from(steps)));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let router = Router::new()
            .fallback(handler)
            .with_state((pending.clone(), seen.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let http = Client::builder()
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build();
        Ok(Self {
            http,
            pending,
            seen,
            server,
        })
    }
    async fn execute(&self, operation: Value) -> anyhow::Result<Result<Value, OperationError>> {
        Ok(serde_json::from_value::<Operation>(operation)?
            .execute(&self.http, Id::new(1))
            .await)
    }
    fn bodies(&self) -> anyhow::Result<Vec<Value>> {
        Ok(self
            .seen
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone())
    }
    fn finished(&self) -> anyhow::Result<()> {
        assert!(
            self.pending
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .is_empty()
        );
        Ok(())
    }
}
type MockState = (Arc<Mutex<VecDeque<Step>>>, Arc<Mutex<Vec<Value>>>);
async fn handler(
    State((pending, seen)): State<MockState>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> axum::response::Response {
    let Ok(mut pending) = pending.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(step) = pending.pop_front() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    assert_eq!(method, step.method);
    assert_eq!(uri.to_string(), step.path);
    if let Ok(mut seen) = seen.lock() {
        seen.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    }
    (step.status, axum::Json(step.body)).into_response()
}
fn get(path: &'static str, body: Value) -> Step {
    Step {
        method: Method::GET,
        path,
        status: StatusCode::OK,
        body,
    }
}
fn channel_fixture(guild: &str) -> Value {
    json!({ "id":"2", "guild_id":guild, "type":0, "name":"test" })
}
fn message_fixture(id: &str) -> Value {
    json!({"id":id,"channel_id":"2","author":{"id":"3","username":"bot","discriminator":"0"},"content":"hello","attachments":[],"embeds":[],"mention_everyone":false,"mention_roles":[],"mentions":[],"pinned":false,"timestamp":"2026-01-01T00:00:00.000000+00:00","tts":false,"type":0})
}

#[tokio::test]
async fn cross_guild_mutation_is_rejected_before_write() -> anyhow::Result<()> {
    let server = Mock::new(vec![get("/api/v10/channels/2", channel_fixture("9"))]).await?;
    let result = server
        .execute(json!({"action":"channels.delete","channel_id":"2"}))
        .await?;
    assert_eq!(result.err().context("should refuse")?.outcome, "rejected");
    server.finished()?;
    assert_eq!(server.bodies()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn history_has_an_honest_cursor_and_preserves_string_ids() -> anyhow::Result<()> {
    serde_json::from_value::<twilight_model::channel::Message>(message_fixture("7"))?;
    let server = Mock::new(vec![
        get("/api/v10/channels/2", channel_fixture("1")),
        get(
            "/api/v10/channels/2/messages?before=8&limit=1",
            json!([message_fixture("7")]),
        ),
    ])
    .await?;
    let result = server
        .execute(json!({"action":"messages.list","channel_id":"2","before":"8","limit":1}))
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert_eq!(result["next_before"], "7");
    assert_eq!(result["may_have_more"], true);
    assert_eq!(result["items"][0]["id"], "7");
    server.finished()
}

#[tokio::test]
async fn send_disables_mentions_and_failed_writes_are_not_replayed() -> anyhow::Result<()> {
    for status in [
        StatusCode::OK,
        StatusCode::FORBIDDEN,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let server = Mock::new(vec![
            get("/api/v10/channels/2", channel_fixture("1")),
            Step {
                method: Method::POST,
                path: "/api/v10/channels/2/messages",
                status,
                body: if status.is_success() {
                    message_fixture("7")
                } else {
                    json!({"code":50013,"message":"upstream secret should not escape"})
                },
            },
        ])
        .await?;
        let result = server
            .execute(json!({"action":"messages.send","channel_id":"2","content":"@everyone <@3>"}))
            .await?;
        let bodies = server.bodies()?;
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["allowed_mentions"]["parse"], json!([]));
        let mentions: AllowedMentions =
            serde_json::from_value(bodies[1]["allowed_mentions"].clone())?;
        assert!(!mentions.replied_user);
        if status.is_success() {
            assert_eq!(
                result.map_err(|e| anyhow::anyhow!("{e:?}"))?["url"],
                "https://discord.com/channels/1/2/7"
            );
        } else {
            let error = result.err().context("should fail")?;
            assert_eq!(
                error.outcome,
                if status == StatusCode::FORBIDDEN {
                    "rejected"
                } else {
                    "uncertain"
                }
            );
            assert!(!error.message.contains("secret"));
        }
        server.finished()?;
    }
    Ok(())
}

#[tokio::test]
async fn permission_edit_uses_one_target_not_the_whole_channel() -> anyhow::Result<()> {
    let role = json!({"id":"4","name":"project","permissions":"0","position":1,"color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"mentionable":false,"flags":0});
    let server = Mock::new(vec![
        get("/api/v10/channels/2", channel_fixture("1")),
        get("/api/v10/guilds/1/roles", json!([role])),
        Step {
            method: Method::PUT,
            path: "/api/v10/channels/2/permissions/4",
            status: StatusCode::NO_CONTENT,
            body: Value::Null,
        },
    ])
    .await?;
    let result=server.execute(json!({"action":"channels.overwrite","channel_id":"2","target_id":"4","kind":"role","allow":"1024","deny":"0"})).await?;
    assert!(result.is_ok(), "{result:?}");
    let bodies = server.bodies()?;
    assert_eq!(bodies[2], json!({"type":0,"allow":"1024","deny":"0"}));
    server.finished()
}

#[tokio::test]
#[expect(clippy::expect_used, reason = "test mutex")]
async fn project_channel_role_thread_and_member_workflow() -> anyhow::Result<()> {
    let role = json!({"id":"7","name":"project","color":0,"colors":{"primary_color":0},"hoist":false,"position":1,"permissions":"1024","managed":false,"mentionable":false,"flags":0});
    serde_json::from_value::<twilight_model::guild::Role>(role.clone())?;
    let thread = json!({"id":"8","guild_id":"1","parent_id":"2","type":11,"name":"discussion"});
    let server = Mock::new(vec![
        Step {
            method: Method::POST,
            path: "/api/v10/guilds/1/channels",
            status: StatusCode::CREATED,
            body: channel_fixture("1"),
        },
        get("/api/v10/channels/2", channel_fixture("1")),
        Step {
            method: Method::PATCH,
            path: "/api/v10/channels/2",
            status: StatusCode::OK,
            body: channel_fixture("1"),
        },
        Step {
            method: Method::POST,
            path: "/api/v10/guilds/1/roles",
            status: StatusCode::OK,
            body: role.clone(),
        },
        get("/api/v10/guilds/1/roles", json!([role.clone()])),
        Step {
            method: Method::PATCH,
            path: "/api/v10/guilds/1/roles/7",
            status: StatusCode::OK,
            body: role.clone(),
        },
        get("/api/v10/guilds/1/roles", json!([role])),
        Step {
            method: Method::PUT,
            path: "/api/v10/guilds/1/members/6/roles/7",
            status: StatusCode::NO_CONTENT,
            body: Value::Null,
        },
        get("/api/v10/channels/2", channel_fixture("1")),
        get("/api/v10/channels/2/messages/5", message_fixture("5")),
        Step {
            method: Method::POST,
            path: "/api/v10/channels/2/messages/5/threads",
            status: StatusCode::CREATED,
            body: thread.clone(),
        },
        get("/api/v10/channels/8", thread.clone()),
        Step {
            method: Method::PATCH,
            path: "/api/v10/channels/8",
            status: StatusCode::OK,
            body: thread,
        },
    ])
    .await?;
    for operation in [
        json!({"action":"channels.create","name":"project","kind":"text"}),
        json!({"action":"channels.update","channel_id":"2","topic":"Work"}),
        json!({"action":"roles.create","name":"project","permissions":"1024"}),
        json!({"action":"roles.update","role_id":"7","color":42}),
        json!({"action":"members.role","user_id":"6","role_id":"7","add":true}),
        json!({"action":"threads.create","channel_id":"2","message_id":"5","name":"discussion"}),
        json!({"action":"threads.update","channel_id":"8","archived":true}),
    ] {
        server
            .execute(operation.clone())
            .await?
            .with_context(|| format!("operation {operation}"))?;
    }
    let bodies = server.seen.lock().expect("test mutex");
    assert_eq!(bodies[2], json!({"topic":"Work"}));
    assert_eq!(bodies[5], json!({"color":42}));
    assert_eq!(bodies[12], json!({"archived":true}));
    Ok(())
}

#[test]
fn wire_fixtures_match_the_extension() -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Fixture {
        operation: Operation,
        mutates: bool,
    }
    let fixtures: Vec<Fixture> = serde_json::from_str(include_str!(
        "../../../../packages/discord-pi/fixtures/operations.json"
    ))?;
    for fixture in fixtures {
        assert_eq!(fixture.operation.mutates(), fixture.mutates);
    }
    Ok(())
}

#[test]
fn strict_operation_inputs_and_attachment_limits() -> anyhow::Result<()> {
    assert!(
        serde_json::from_value::<Operation>(
            json!({"action":"members.ban","user_id":"3","banned":true,"surprise":true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<Operation>(json!({"action":"channels.delete","channel_id":"0"}))
            .is_err()
    );
    assert!(
        uploads(vec![Upload {
            filename: "../secret".into(),
            data: "YQ==".into()
        }])
        .is_err()
    );
    assert!(
        uploads(vec![Upload {
            filename: "ok".into(),
            data: "not base64".into()
        }])
        .is_err()
    );
    assert!(
        uploads(vec![Upload {
            filename: "ok".into(),
            data: STANDARD.encode(vec![0; 4 * 1024 * 1024 + 1])
        }])
        .is_err()
    );
    assert_eq!(
        uploads(vec![Upload {
            filename: "hello.txt".into(),
            data: "aGVsbG8=".into()
        }])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .len(),
        1
    );
    Ok(())
}
