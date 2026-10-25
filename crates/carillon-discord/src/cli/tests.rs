use std::{fs, path::Path};

use anyhow::{Context as _, Result};
use carillon_installation::config::{LoadedConfig, Provider, ProviderRole};

use super::{Cli, Command, call, instance, uploads};
use crate::operations::{Messages, Operation};
use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse as _,
};
use clap::{Parser as _, error::ErrorKind};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn parsed_call(arguments: &[&str]) -> Result<Operation> {
    let cli = Cli::try_parse_from(
        ["carillon-discord", "call"]
            .into_iter()
            .chain(arguments.iter().copied()),
    )?;
    let Command::Call { operation, .. } = cli.command else {
        anyhow::bail!("not a call");
    };
    Ok(operation)
}

fn refused_call(arguments: &[&str]) -> Result<clap::Error> {
    parsed_call(arguments)
        .err()
        .context("the call parsed")?
        .downcast()
}

/// A provider at a loopback address that reads each request and gives
/// `answer`, or closes without one. It counts the requests it received.
async fn applying_provider(
    answer: Option<&'static [u8]>,
) -> Result<(super::Operator, tokio::task::JoinHandle<usize>)> {
    answering_after(answer.map(<[u8]>::to_vec), std::time::Duration::ZERO).await
}

/// [`applying_provider`], answering each request `delay` after reading it.
async fn answering_after(
    answer: Option<Vec<u8>>,
    delay: std::time::Duration,
) -> Result<(super::Operator, tokio::task::JoinHandle<usize>)> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
    let operator = super::Operator {
        url: format!("http://{}/v1/settings", listener.local_addr()?),
        credential: "operator".to_owned(),
    };
    let server = tokio::spawn(async move {
        let mut requests = 0;
        while let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await
        {
            requests += 1;
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let Ok(byte) = stream.read_u8().await else {
                    break;
                };
                request.push(byte);
            }
            tokio::time::sleep(delay).await;
            if let Some(answer) = &answer {
                let _ = stream.write_all(answer).await;
            }
        }
        requests
    });
    Ok((operator, server))
}

#[tokio::test]
async fn a_settings_apply_is_sent_once_and_only_an_answer_confirms_it() -> Result<()> {
    // Nothing answering at the provider's configured address does not
    // confirm it is stopped.
    let (mut operator, server) = applying_provider(None).await?;
    server.abort();
    let unused = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
    operator.url = format!("http://{}/v1/settings", unused.local_addr()?);
    drop(unused);
    assert!(matches!(
        operator.apply_settings().await?,
        super::Applying::Unreachable
    ));
    // A refusal is the provider's answer. A conflict it does not mark as
    // one a restart resolves, as when maintenance is busy, is not one.
    let refusal: &[u8] =
        b"HTTP/1.1 409 Conflict\r\nContent-Length: 25\r\n\r\n{\"error\":\"guild changed\"}";
    let (operator, server) = applying_provider(Some(refusal)).await?;
    let super::Applying::Refused(refused) = operator.apply_settings().await? else {
        anyhow::bail!("a refusal was not reported as one");
    };
    assert_eq!(refused.reason, "guild changed");
    assert!(refused.restart.is_empty());
    assert_eq!(server.await?, 1);
    let body = json!({
        "error": "only a restart applies (guild_id)",
        "serving": {"source": "bundled", "commands": [], "digest": "d"},
        "restart": ["guild_id"],
    })
    .to_string();
    let answer = format!(
        "HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (operator, server) =
        answering_after(Some(answer.into_bytes()), std::time::Duration::ZERO).await?;
    let super::Applying::Refused(refused) = operator.apply_settings().await? else {
        anyhow::bail!("a refusal was not reported as one");
    };
    assert_eq!(refused.restart, ["guild_id"]);
    assert_eq!(server.await?, 1);
    // A request the provider received without answering may have been
    // applied: it is reported as unconfirmed and not sent again.
    let (operator, server) = applying_provider(None).await?;
    let unconfirmed = operator
        .apply_settings()
        .await
        .err()
        .context("an unanswered apply was confirmed")?;
    assert!(
        unconfirmed.to_string().contains("did not confirm"),
        "{unconfirmed}"
    );
    assert_eq!(server.await?, 1);
    Ok(())
}

/// An apply that registers a changed module's commands can take longer
/// than a status read; it is waited for as a change, and sent once.
#[tokio::test]
async fn an_apply_slower_than_a_status_read_is_still_confirmed() -> Result<()> {
    let summary =
        |digest: &str| json!({"source": "bundled", "commands": ["model"], "digest": digest});
    let body = json!({
        "changed": ["module"],
        "module": {"serving": summary("new"), "previous": summary("old"), "discord": "registered"},
    })
    .to_string();
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (operator, server) = answering_after(
        Some(answer.into_bytes()),
        super::STATUS_TIMEOUT + std::time::Duration::from_secs(1),
    )
    .await?;
    let super::Applying::Applied(applied) = operator.apply_settings().await? else {
        anyhow::bail!("a slow apply was not confirmed");
    };
    assert_eq!(applied.changed, ["module"]);
    assert_eq!(server.await?, 1);
    Ok(())
}

#[test]
fn call_preserves_large_ids_and_explicit_false_flags() -> Result<()> {
    let operation = parsed_call(&[
        "messages",
        "pin",
        "--channel-id",
        "1557758596627038269",
        "--message-id",
        "1557758596627038270",
        "--pinned",
        "false",
    ])?;
    let encoded = serde_json::to_value(&operation)?;
    assert_eq!(
        encoded,
        json!({
            "action": "messages.pin", "channel_id": "1557758596627038269",
            "message_id": "1557758596627038270", "pinned": false,
        })
    );
    let decoded: Operation = serde_json::from_value(encoded)?;
    assert!(matches!(
        decoded,
        Operation::Messages(Messages::Pin { pinned: false, .. })
    ));
    Ok(())
}

#[test]
fn call_requires_the_intent_of_toggle_mutations_and_valid_ids() -> Result<()> {
    let unpinned = refused_call(&["messages", "pin", "--channel-id", "2", "--message-id", "3"])?;
    assert_eq!(unpinned.kind(), ErrorKind::MissingRequiredArgument);
    assert!(unpinned.to_string().contains("--pinned"), "{unpinned}");
    let unbanned = refused_call(&["members", "ban", "--user-id", "3"])?;
    assert_eq!(unbanned.kind(), ErrorKind::MissingRequiredArgument);
    assert!(unbanned.to_string().contains("--banned"), "{unbanned}");
    assert_eq!(
        refused_call(&["messages", "get", "--channel-id", "0", "--message-id", "3"])?.kind(),
        ErrorKind::ValueValidation
    );
    Ok(())
}

struct CallProvider {
    config: LoadedConfig,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    directory: tempfile::TempDir,
}

impl Drop for CallProvider {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl CallProvider {
    async fn new(status: StatusCode, answer: String) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let mut config =
            crate::config::tests::installation(directory.path(), "discord-private", 46_400)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = config
            .value
            .providers
            .iter_mut()
            .find(|provider| provider.id == "discord-private")
            .context("missing provider")?;
        target.url = format!("http://{}", listener.local_addr()?);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let router = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let requests = requests.clone();
            let answer = answer.clone();
            async move {
                let recorded = json!({
                    "authorization": headers.get("authorization").and_then(|value| value.to_str().ok()),
                    "operation": serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
                });
                if let Ok(mut requests) = requests.lock() {
                    requests.push((uri.to_string(), recorded));
                }
                let mut response = (status, answer).into_response();
                if status.is_redirection() {
                    response.headers_mut().insert("location", axum::http::HeaderValue::from_static("/redirected"));
                }
                response
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(Self {
            config,
            seen,
            server,
            directory,
        })
    }

    fn requests(&self) -> Result<Vec<(String, Value)>> {
        Ok(self
            .seen
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned requests"))?
            .clone())
    }
}

#[tokio::test]
async fn call_authenticates_at_the_existing_endpoint_and_returns_native_json() -> Result<()> {
    let provider = CallProvider::new(
        StatusCode::OK,
        json!({"messages": [], "may_have_more": false}).to_string(),
    )
    .await?;
    let operation = parsed_call(&["messages", "list", "--channel-id", "2", "--limit", "7"])?;
    let result = call(&provider.config, "discord-private", operation).await?;
    assert_eq!(result, json!({"messages": [], "may_have_more": false}));
    assert_eq!(
        provider.requests()?,
        vec![(
            "/v1/operations".into(),
            json!({
                "authorization": "Bearer operator",
                "operation": {"action": "messages.list", "channel_id": "2", "before": null, "limit": 7},
            })
        )]
    );
    Ok(())
}

#[tokio::test]
async fn call_never_retries_an_uncertain_write_or_follows_a_redirect() -> Result<()> {
    for (status, answer) in [
        (
            StatusCode::BAD_GATEWAY,
            json!({"outcome": "uncertain", "message": "inspect the message"}).to_string(),
        ),
        (StatusCode::FOUND, json!({"error": "moved"}).to_string()),
    ] {
        let provider = CallProvider::new(status, answer).await?;
        let operation = parsed_call(&[
            "messages",
            "delete",
            "--channel-id",
            "2",
            "--message-id",
            "3",
        ])?;
        let error = call(&provider.config, "discord-private", operation)
            .await
            .err()
            .context("write should fail")?;
        assert!(error.to_string().starts_with("uncertain:"), "{error}");
        assert_eq!(provider.requests()?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn an_unreadable_answer_preserves_write_uncertainty_without_replaying() -> Result<()> {
    for answer in ["not JSON".to_owned(), "x".repeat(3 * 1024 * 1024)] {
        let provider = CallProvider::new(StatusCode::OK, answer).await?;
        let operation = parsed_call(&[
            "messages",
            "send",
            "--channel-id",
            "2",
            "--content",
            "hello",
        ])?;
        let error = call(&provider.config, "discord-private", operation)
            .await
            .err()
            .context("invalid answer should fail")?;
        assert!(error.to_string().starts_with("uncertain:"), "{error}");
        assert_eq!(provider.requests()?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn a_refusal_without_a_readable_body_keeps_its_status() -> Result<()> {
    // An older provider's framework rejects an operation it does not
    // know in plain text; a missing route answers with no body at all.
    for (status, answer, outcome) in [
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Failed to deserialize the JSON body",
            "rejected",
        ),
        (StatusCode::NOT_FOUND, "", "rejected"),
        (StatusCode::BAD_GATEWAY, "upstream failed", "uncertain"),
    ] {
        let provider = CallProvider::new(status, answer.to_owned()).await?;
        let operation = parsed_call(&[
            "messages",
            "send",
            "--channel-id",
            "2",
            "--content",
            "hello",
        ])?;
        let error = call(&provider.config, "discord-private", operation)
            .await
            .err()
            .context("a refusal succeeded")?
            .to_string();
        assert!(error.starts_with(&format!("{outcome}:")), "{error}");
        assert!(error.contains(status.as_str()), "{error}");
        assert_eq!(provider.requests()?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn upload_rejections_happen_before_any_http_operation() -> Result<()> {
    let provider = CallProvider::new(StatusCode::OK, "{}".into()).await?;
    let file = provider.directory.path().join("oversize");
    fs::File::create(&file)?.set_len(5 * 1024 * 1024)?;
    let path = file.to_str().context("temporary path")?;
    let send = |files: &[&str]| {
        let mut arguments = vec!["messages", "send", "--channel-id", "2", "--content", "hi"];
        for file in files {
            arguments.extend(["--file", file]);
        }
        parsed_call(&arguments)
    };
    let refused = call(&provider.config, "discord-private", send(&[path])?)
        .await
        .err()
        .context("an oversized file was sent")?;
    assert!(refused.to_string().contains("4 MiB"), "{refused}");
    let small = provider.directory.path().join("small");
    fs::write(&small, "x")?;
    let small = small.to_str().context("temporary path")?;
    let refused = call(&provider.config, "discord-private", send(&[small; 11])?)
        .await
        .err()
        .context("eleven files were sent")?;
    assert!(refused.to_string().contains("At most 10"), "{refused}");
    assert!(provider.requests()?.is_empty());
    let directory = uploads(&[provider.directory.path().to_path_buf()])
        .err()
        .context("a directory was uploaded")?;
    assert_eq!(
        directory.to_string(),
        "attachments must be regular files; nothing was changed"
    );
    // Only a message send takes files.
    assert_eq!(
        refused_call(&[
            "messages",
            "get",
            "--channel-id",
            "2",
            "--message-id",
            "3",
            "--file",
            small
        ])?
        .kind(),
        ErrorKind::UnknownArgument
    );
    assert_eq!(
        refused_call(&["--file", small, "messages", "send", "--channel-id", "2"])?.kind(),
        ErrorKind::UnknownArgument
    );
    Ok(())
}

#[test]
fn a_timeout_is_set_or_cleared_explicitly() -> Result<()> {
    let of_user = |arguments: &[&'static str]| {
        [&["members", "timeout", "--user-id", "3"], arguments].concat()
    };
    let timeout = |arguments| anyhow::Ok(serde_json::to_value(parsed_call(&of_user(arguments))?)?);
    let refused = |arguments| refused_call(&of_user(arguments)).map(|failure| failure.kind());
    assert_eq!(
        refused(&[])?,
        ErrorKind::MissingRequiredArgument,
        "no expiry must not remove a timeout"
    );
    assert_eq!(
        refused(&["--clear", "--until", "2030-01-01T00:00:00Z"])?,
        ErrorKind::ArgumentConflict
    );
    assert_eq!(
        timeout(&["--clear"])?,
        json!({"action": "members.timeout", "user_id": "3", "until": null})
    );
    assert_eq!(
        timeout(&["--until", "2030-01-01T00:00:00Z"])?,
        json!({"action": "members.timeout", "user_id": "3", "until": "2030-01-01T00:00:00Z"})
    );
    // On the wire, too, only an explicit null clears.
    let unset =
        serde_json::from_value::<Operation>(json!({"action": "members.timeout", "user_id": "3"}))
            .err()
            .context("a timeout without until decoded")?;
    assert_eq!(
        unset.to_string(),
        "data did not match any variant of untagged enum Operation"
    );
    Ok(())
}

#[test]
fn the_provider_is_named_before_or_after_the_operation() -> Result<()> {
    for arguments in [
        ["--provider", "discord-b", "server", "info"],
        ["server", "info", "--provider", "discord-b"],
    ] {
        let cli = Cli::try_parse_from(["carillon-discord", "call"].into_iter().chain(arguments))?;
        let Command::Call { provider, .. } = cli.command else {
            anyhow::bail!("not a call");
        };
        assert_eq!(provider.as_deref(), Some("discord-b"));
    }
    Ok(())
}

#[tokio::test]
async fn message_files_are_loaded_locally_and_sent_as_bounded_bytes() -> Result<()> {
    let provider = CallProvider::new(StatusCode::OK, json!({"id": "3"}).to_string()).await?;
    let file = provider.directory.path().join("evidence.txt");
    fs::write(&file, b"hello\n")?;
    let operation = parsed_call(&[
        "messages",
        "send",
        "--channel-id",
        "2",
        "--content",
        "evidence",
        "--file",
        file.to_str().context("temporary path")?,
    ])?;
    call(&provider.config, "discord-private", operation).await?;
    let requests = provider.requests()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].1["operation"]["attachments"],
        json!([
            {"filename": "evidence.txt", "data": "aGVsbG8K"}
        ])
    );
    Ok(())
}

#[tokio::test]
async fn a_module_declaration_changes_only_outside_other_maintenance() -> Result<()> {
    use super::{ModuleAction, manage_module};
    use carillon_installation::maintenance::MaintenanceLock;

    let directory = tempfile::tempdir()?;
    let mut config =
        crate::config::tests::installation(directory.path(), "discord-private", 46_500)?;
    // Nothing answers at the provider's address.
    let unused = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
    config
        .value
        .providers
        .iter_mut()
        .find(|provider| provider.id == "discord-private")
        .context("missing provider")?
        .url = format!("http://{}", unused.local_addr()?);
    drop(unused);
    let module = directory.path().join("custom.wasm");
    crate::module::tests::write_module(
        &module,
        &crate::module::tests::describing(&[crate::module::tests::definition("model")]),
        &crate::module::tests::replying("custom", &[]),
    )?;
    let settings = config.provider_config_path("discord-private")?;
    let original = fs::read(&settings)?;
    let held = MaintenanceLock::acquire(&config.root)?;
    for action in [
        ModuleAction::Enable {
            file: Some(module.clone()),
        },
        ModuleAction::Disable,
    ] {
        let refused = manage_module(|| Ok(config.clone()), None, Some(action))
            .await
            .err()
            .context("changed the declaration during other maintenance")?;
        assert!(
            format!("{refused:#}").contains("another Carillon maintenance operation"),
            "{refused:#}"
        );
        assert_eq!(fs::read(&settings)?, original);
    }
    drop(held);
    // The declaration changes, and the lock is free again for the
    // provider's own change.
    manage_module(
        || Ok(config.clone()),
        None,
        Some(ModuleAction::Enable {
            file: Some(module.clone()),
        }),
    )
    .await?;
    let declared = crate::config::DiscordConfig::read(&config, "discord-private")?.module;
    assert!(declared.enabled);
    assert_eq!(declared.path, Some(module));
    assert!(MaintenanceLock::try_acquire(&config.root)?.is_some());
    manage_module(|| Ok(config.clone()), None, Some(ModuleAction::Disable)).await?;
    assert!(
        !crate::config::DiscordConfig::read(&config, "discord-private")?
            .module
            .enabled
    );
    Ok(())
}

/// After setup or `configure`, a refusal of what only a restart applies
/// waits for `carillon restart` only when this release would serve the
/// saved file; any other refusal is an error, the file saved.
#[tokio::test]
async fn saved_settings_wait_for_a_restart_only_when_it_would_serve_them() -> Result<()> {
    use crate::module::tests::{definition, describing, module, replying};

    let directory = tempfile::tempdir()?;
    let config = crate::config::tests::installation(
        &directory.path().join("installation"),
        "discord-private",
        46_720,
    )?;
    let refused = |restart: &[&str]| super::Refused {
        reason: "the settings file changes what only a restart applies".to_owned(),
        restart: restart.iter().map(|field| (*field).to_owned()).collect(),
    };
    let outcome = |restart: &[&str]| {
        let config = config.clone();
        let refused = refused(restart);
        async move {
            super::saved_but_refused(&config, "discord-private", refused)
                .await
                .map_err(|error| format!("{error:#}"))
        }
    };
    let declare = |path: Option<&Path>| -> Result<()> {
        let mut discord = crate::config::DiscordConfig::read(&config, "discord-private")?;
        discord.module.enabled = path.is_some();
        discord.module.path = path.map(Path::to_path_buf);
        discord.write(&config, "discord-private")
    };

    // Fields only a restart applies, with the bundled module or a usable
    // custom one: the saved file waits for `carillon restart`.
    outcome(&["guild_id"]).await.map_err(anyhow::Error::msg)?;
    let custom = directory.path().join("custom.wasm");
    fs::write(
        &custom,
        module(&describing(&[definition("model")]), &replying("ok", &[]))?,
    )?;
    declare(Some(&custom))?;
    outcome(&["guild_id"]).await.map_err(anyhow::Error::msg)?;
    // The provider did not try the module: one rebuilt for another
    // contract, or deleted, would keep a restart from starting it.
    for broken in [Some(&b"\0asm old contract"[..]), None] {
        match broken {
            Some(bytes) => fs::write(&custom, bytes)?,
            None => fs::remove_file(&custom)?,
        }
        let refusal = outcome(&["guild_id"])
            .await
            .err()
            .context("a restart was advised for an unusable module")?;
        assert!(
            refusal.starts_with(
                "the settings file is saved, but `carillon restart` would not start \
                 discord-private: "
            ),
            "{refusal}"
        );
    }
    // Any other refusal leaves the file saved but not served.
    declare(None)?;
    let refusal = outcome(&[])
        .await
        .err()
        .context("a refusal was reported as served")?;
    assert_eq!(
        refusal,
        "the settings file is saved, but discord-private did not serve it: the settings \
         file changes what only a restart applies; it keeps serving the settings it has"
    );
    Ok(())
}

/// An update's preflight runs the new release's program: a module the
/// installed application ships is checked as the new release ships it,
/// and any other module as it is.
#[tokio::test]
async fn a_declared_module_is_checked_where_the_update_takes_it_from() -> Result<()> {
    use crate::{
        config::DiscordConfig,
        module::tests::{definition, describing, module, replying},
    };

    let directory = tempfile::tempdir()?;
    let config = crate::config::tests::installation(
        &directory.path().join("installation"),
        "discord-private",
        46_700,
    )?;
    let release = directory.path().join("release");
    let shipped = Path::new("lib/examples/card-colours.wasm");
    for root in [config.root.join("app"), release.clone()] {
        fs::create_dir_all(root.join("lib/examples"))?;
    }
    let usable = module(&describing(&[definition("model")]), &replying("ok", &[]))?;
    let declare = |path: &Path| -> Result<DiscordConfig> {
        let mut discord = DiscordConfig::read(&config, "discord-private")?;
        discord.module.enabled = true;
        discord.module.path = Some(path.to_path_buf());
        Ok(discord)
    };
    let check = |discord: DiscordConfig| {
        let (config, release) = (config.clone(), release.clone());
        async move {
            super::check_declared(&config, "discord-private", &discord, &release)
                .await
                .map_err(|error| format!("{error:#}"))
        }
    };

    // The installed copy is the old release's; the new release's serves.
    let installed = config.root.join("app").join(shipped);
    fs::write(&installed, b"\0asm old contract")?;
    fs::write(release.join(shipped), &usable)?;
    check(declare(&installed)?)
        .await
        .map_err(anyhow::Error::msg)?;
    // A relative declaration starts at the installation, inside `app/` too.
    check(declare(&Path::new("app").join(shipped))?)
        .await
        .map_err(anyhow::Error::msg)?;

    // A usable installed copy does not hide a new release that cannot serve.
    fs::write(&installed, &usable)?;
    fs::write(release.join(shipped), b"\0asm broken")?;
    let refusal = check(declare(&installed)?).await.err().unwrap_or_default();
    assert!(refusal.contains("cannot serve"), "{refusal}");
    fs::remove_file(release.join(shipped))?;
    let refusal = check(declare(&installed)?).await.err().unwrap_or_default();
    assert!(refusal.contains("read Discord module"), "{refusal}");

    // A detour that stays inside the application is still the release's.
    fs::write(release.join(shipped), &usable)?;
    fs::write(&installed, b"\0asm old contract")?;
    check(declare(
        &config
            .root
            .join("app/lib/../lib/examples/card-colours.wasm"),
    )?)
    .await
    .map_err(anyhow::Error::msg)?;
    // One that leads out of it is the owner's file, never the release's
    // path spelt the same way.
    let modules = config.root.join("modules");
    fs::create_dir_all(&modules)?;
    fs::create_dir_all(directory.path().join("modules"))?;
    fs::write(modules.join("custom.wasm"), b"\0asm broken")?;
    fs::write(directory.path().join("modules/custom.wasm"), &usable)?;
    let escaping = Path::new("app/../modules/custom.wasm");
    let refusal = check(declare(escaping)?).await.err().unwrap_or_default();
    assert!(refusal.contains("cannot serve"), "{refusal}");
    fs::write(modules.join("custom.wasm"), &usable)?;
    check(declare(escaping)?)
        .await
        .map_err(anyhow::Error::msg)?;

    // A module of the owner's own is checked as it is, whatever the
    // release ships.
    fs::write(release.join(shipped), &usable)?;
    let own = directory.path().join("own.wasm");
    fs::write(&own, b"\0asm broken")?;
    let refusal = check(declare(&own)?).await.err().unwrap_or_default();
    assert!(refusal.contains("cannot serve"), "{refusal}");
    fs::write(&own, &usable)?;
    check(declare(&own)?).await.map_err(anyhow::Error::msg)?;
    Ok(())
}

#[test]
fn only_an_unambiguous_discord_instance_is_inferred() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut config = crate::config::tests::installation(directory.path(), "discord-a", 46_300)?;
    assert_eq!(instance(&config, None)?, "discord-a");
    assert_eq!(instance(&config, Some("chosen".to_owned()))?, "chosen");

    // Another platform's interface has its own settings, not a Discord choice.
    let mut other = config.value.providers[0].clone();
    "slack".clone_into(&mut other.id);
    config.value.providers.push(other);
    fs::write(
        config.provider_config_path("slack")?,
        "workspace = \"T1\"\n",
    )?;
    assert_eq!(instance(&config, None)?, "discord-a");

    let second = Provider {
        id: "discord-b".to_owned(),
        role: ProviderRole::Interface,
        ..config.value.providers[0].clone()
    };
    config.value.providers.push(second);
    fs::copy(
        config.provider_config_path("discord-a")?,
        config.provider_config_path("discord-b")?,
    )?;
    let ambiguous = instance(&config, None)
        .err()
        .context("inferred one of several instances")?;
    assert!(
        ambiguous.to_string().ends_with("discord-a, discord-b"),
        "{ambiguous}"
    );

    for id in ["discord-a", "discord-b"] {
        fs::remove_file(config.provider_config_path(id)?)?;
    }
    let unconfigured = instance(&config, None)
        .err()
        .context("inferred an unconfigured instance")?;
    assert!(
        unconfigured
            .to_string()
            .ends_with("discord-a, slack, discord-b"),
        "{unconfigured}"
    );
    Ok(())
}
