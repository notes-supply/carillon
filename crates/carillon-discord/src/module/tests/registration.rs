use super::*;

#[tokio::test]
async fn loading_checks_the_contract_and_definitions() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let release = directory.path().join("release.wasm");
    let reply = replying("unused", &[]);
    let contract = carillon_discord_module::contract::CONTRACT;
    for (description, refused) in [
            (
                json!({"Description": {"contract": contract + 1, "commands": []}}).to_string(),
                format!(
                    "Discord module speaks contract {}; this provider speaks {contract}",
                    contract + 1
                ),
            ),
            (
                describing(&[json!({"name": "missing-type"})]),
                "invalid command definition: missing field `description`".to_owned(),
            ),
            (
                reply.clone(),
                "Discord module did not describe itself".to_owned(),
            ),
            (
                "not json".to_owned(),
                "describe Discord module: decode Discord module outcome: expected value at line 1 column 1"
                    .to_owned(),
            ),
        ] {
            let bytes = module(&description, &reply)?;
            let checked = super::super::check(&bytes).await.err().context("check accepted")?;
            assert_eq!(format!("{checked:#}"), refused);
            std::fs::write(&release, bytes)?;
            let loaded = Module::load(files(directory.path()), Source::Bundled)
                .await
                .err()
                .context("load accepted")?;
            assert_eq!(
                format!("{loaded:#}"),
                format!(
                    "bundled Discord module {} is unusable: {refused}",
                    release.display()
                )
            );
        }
    let broken = super::super::check(b"\0asm broken")
        .await
        .err()
        .context("a broken module was accepted")?;
    assert!(
        format!("{broken:#}").starts_with("compile guest module: "),
        "{broken:#}"
    );
    assert_eq!(
        super::super::check(&variant(&["model"], "unused")?).await?,
        ["model"]
    );
    let commands = release_commands(directory.path(), &["model"], "unused").await?;
    assert_eq!(commands.serving().source, Source::Bundled);
    assert_eq!(commands.serving().commands, ["model"]);
    Ok(())
}

#[tokio::test]
async fn applying_serves_only_usable_modules_and_rereads_the_declared_file() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    let path = root.join("custom.wasm");
    let declared = custom(root, "custom.wasm");

    // A missing or unusable candidate changes nothing.
    let unread: fn(&Declared) -> bool = |declared| matches!(declared, Declared::Unknown { .. });
    let other: fn(&Declared) -> bool = |declared| {
        matches!(
            declared,
            Declared::Other {
                source: Source::Custom { .. }
            }
        )
    };
    for (bytes, compared) in [(None, unread), (Some(b"\0asm broken".as_slice()), other)] {
        if let Some(bytes) = bytes {
            std::fs::write(&path, bytes)?;
        }
        assert!(matches!(
            commands.apply(declared.clone(), None).await,
            Err(ChangeError::Unusable(_))
        ));
        assert_eq!(reply(&commands).await?, "release");
        let found = commands.declared(declared.clone());
        assert!(compared(&found), "{found:?}");
    }
    assert!(MaintenanceLock::try_acquire(root)?.is_some());

    let first = variant(&["model"], "custom")?;
    std::fs::write(&path, &first)?;
    let change = commands.apply(declared.clone(), None).await?;
    assert_eq!(change.previous.source, Source::Bundled);
    assert_eq!(change.serving.source, declared);
    assert_eq!(change.discord, Registration::NotConnected);
    assert_eq!(reply(&commands).await?, "custom");
    assert_eq!(commands.declared(declared.clone()), Declared::Serving);
    let again = commands.apply(declared.clone(), None).await?;
    assert_eq!(again.previous, again.serving);

    // A rebuild at the same path is another module until it is applied.
    std::fs::write(&path, variant(&["model", "other"], "rebuilt")?)?;
    assert_eq!(
        commands.declared(declared.clone()),
        Declared::Other {
            source: declared.clone()
        }
    );
    assert_eq!(reply(&commands).await?, "custom");
    let change = commands.apply(declared.clone(), None).await?;
    assert_eq!(change.serving.commands, ["model", "other"]);
    assert_ne!(change.serving.digest, change.previous.digest);
    assert_eq!(reply(&commands).await?, "rebuilt");

    // A start serves the declared module.
    let restarted = Module::load(files(root), declared.clone()).await?;
    assert_eq!(restarted.serving(), change.serving);
    assert_eq!(reply(&restarted).await?, "rebuilt");

    // A rebuild that breaks keeps the module serving, and refuses a start,
    // naming the file and how to serve the bundled module instead.
    std::fs::write(&path, b"\0asm broken")?;
    assert!(matches!(
        commands.apply(declared.clone(), None).await,
        Err(ChangeError::Unusable(_))
    ));
    assert_eq!(reply(&commands).await?, "rebuilt");
    let failure = Module::load(files(root), declared.clone())
        .await
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(
        failure.contains(&format!("custom Discord module {}", path.display()))
            && failure.contains("disable it"),
        "{failure}"
    );

    // Disabled, the bundled module serves.
    let change = commands.apply(Source::Bundled, None).await?;
    assert_eq!(change.serving.source, Source::Bundled);
    assert_eq!(reply(&commands).await?, "release");
    Ok(())
}

#[tokio::test]
async fn changes_refused_during_other_maintenance_change_nothing() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    let status = Arc::new(AtomicU16::new(200));
    let (address, server, requests) = counting_registration_server(status).await?;
    let target = app(&address.to_string(), 1, 100);
    std::fs::write(root.join("custom.wasm"), variant(&["other"], "custom")?)?;
    let held = MaintenanceLock::acquire(root)?;
    // An apply that changes nothing needs no lock.
    let unchanged = commands.apply(Source::Bundled, None).await?;
    assert_eq!(unchanged.previous, unchanged.serving);
    for source in [custom(root, "custom.wasm"), Source::Bundled] {
        assert!(matches!(
            commands.apply(source, Some(&target)).await,
            Err(ChangeError::Busy(_))
        ));
    }
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(reply(&commands).await?, "release");
    drop(held);
    commands
        .apply(custom(root, "custom.wasm"), Some(&target))
        .await?;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    // The provider releases the lock once its change is complete.
    assert!(MaintenanceLock::try_acquire(root)?.is_some());
    server.abort();
    Ok(())
}

#[tokio::test]
async fn refused_registration_keeps_the_module_and_forgets_the_record() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    let status = Arc::new(AtomicU16::new(200));
    let (address, server, requests) = counting_registration_server(status.clone()).await?;
    let target = app(&address.to_string(), 1, 100);
    assert_eq!(commands.reconcile(&target).await?, Registration::Registered);
    assert_eq!(commands.reconcile(&target).await?, Registration::Unchanged);
    let registered =
        |commands: &Module| commands.status(Some(&target), Declared::Serving).registered;
    assert_eq!(registered(&commands), Some(true));

    std::fs::write(
        root.join("custom.wasm"),
        variant(&["model", "other"], "custom")?,
    )?;
    for code in [400, 500] {
        status.store(code, Ordering::SeqCst);
        assert!(matches!(
            commands
                .apply(custom(root, "custom.wasm"), Some(&target))
                .await,
            Err(ChangeError::Register(_))
        ));
        assert_eq!(reply(&commands).await?, "release");
        // Discord may hold either definition set now, so the record
        // attests to neither and the next change or start registers.
        assert!(!files(root).record.exists());
        assert_eq!(registered(&commands), Some(false));
    }

    // Applying the serving module again reconciles Discord.
    status.store(200, Ordering::SeqCst);
    let before = requests.load(Ordering::SeqCst);
    let change = commands.apply(Source::Bundled, Some(&target)).await?;
    assert_eq!(change.previous, change.serving);
    assert_eq!(change.discord, Registration::Registered);
    assert_eq!(requests.load(Ordering::SeqCst), before + 1);
    assert_eq!(registered(&commands), Some(true));
    let change = commands.apply(Source::Bundled, Some(&target)).await?;
    assert_eq!(change.discord, Registration::Unchanged);
    assert_eq!(requests.load(Ordering::SeqCst), before + 1);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_record_that_cannot_be_cleared_stops_before_discord() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    let (address, server, requests) =
        counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
    let target = app(&address.to_string(), 1, 100);
    std::fs::write(root.join("custom.wasm"), variant(&["other"], "custom")?)?;
    // A directory where the record belongs cannot be removed as a file.
    std::fs::create_dir_all(files(root).record.join("occupied"))?;
    assert!(matches!(
        commands
            .apply(custom(root, "custom.wasm"), Some(&target))
            .await,
        Err(ChangeError::Record(
            carillon_installation::filesystem::PublishError::NotPublished(_)
        ))
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(reply(&commands).await?, "release");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn overlapping_applies_register_and_activate_in_order() -> anyhow::Result<()> {
    use tokio::sync::{Semaphore, mpsc};

    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["initial"], "initial").await?;
    let (sent, mut received) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target = app(&listener.local_addr()?.to_string(), 1, 100);
    let router = gated_registration(sent, release.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(root.join("first.wasm"), variant(&["first"], "first")?)?;
    std::fs::write(root.join("second.wasm"), variant(&["second"], "second")?)?;
    let first = commands.apply(custom(root, "first.wasm"), Some(&target));
    tokio::pin!(first);
    let registration = tokio::select! {
        _ = &mut first => anyhow::bail!("apply completed before confirmation"),
        request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
    };
    assert_eq!(registration[0]["name"], "first");
    let second = commands.apply(custom(root, "second.wasm"), Some(&target));
    tokio::pin!(second);
    // Poll both operations while registration is blocked. The second
    // must not send a request or activate; ordinary invocations still work.
    tokio::select! {
        _ = &mut first => anyhow::bail!("first activated before confirmation"),
        _ = &mut second => anyhow::bail!("second bypassed the first apply"),
        request = received.recv() => anyhow::bail!("overlapping registration: {request:?}"),
        () = tokio::time::sleep(Duration::from_millis(100)) => {},
    }
    assert_eq!(reply(&commands).await?, "initial");
    release.add_permits(1);
    assert_eq!(first.await?.discord, Registration::Registered);
    assert_eq!(commands.serving().commands, ["first"]);
    assert_eq!(reply(&commands).await?, "first");
    let registration = tokio::select! {
        _ = &mut second => anyhow::bail!("second completed before confirmation"),
        request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
    };
    assert_eq!(registration[0]["name"], "second");
    release.add_permits(1);
    assert_eq!(second.await?.discord, Registration::Registered);
    assert_eq!(commands.serving().commands, ["second"]);
    assert_eq!(
        commands.status(Some(&target), Declared::Serving).registered,
        Some(true)
    );
    assert_eq!(reply(&commands).await?, "second");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_interaction_under_way_finishes_with_the_module_it_began_with() -> anyhow::Result<()> {
    use axum::routing::get;
    use tokio::sync::{Semaphore, mpsc};

    let directory = tempfile::tempdir()?;
    let root = directory.path();
    std::fs::write(root.join("release.wasm"), running_module()?)?;
    let commands = Arc::new(Module::load(files(root), Source::Bundled).await?);
    let (asked, mut asking) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let router = Router::new().route(
        "/v1/commands",
        get(move || {
            let (asked, release) = (asked.clone(), gate.clone());
            async move {
                let _ = asked.send(());
                let _permit = release.acquire().await;
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let core = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let calls = super::super::InteractionCalls::new(
        crate::client::test_client(core)?,
        carillon_protocol::model::ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: None,
        },
    );
    let under_way = tokio::spawn({
        let commands = commands.clone();
        async move { commands.invoke(calls, interaction()).await }
    });
    asking
        .recv()
        .await
        .context("the interaction never asked core")?;
    std::fs::write(
        root.join("custom.wasm"),
        variant(&["model"], "replacement")?,
    )?;
    commands.apply(custom(root, "custom.wasm"), None).await?;
    assert_eq!(reply(&commands).await?, "replacement");
    release.add_permits(1);
    assert_eq!(under_way.await??.content, "ran");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn registration_is_recorded_for_its_target_once_confirmed() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let commands = release_commands(directory.path(), &["model"], "unused").await?;
    let record = files(directory.path()).record;
    let definitions = commands.current().definitions.clone();
    let (address, server, requests) =
        counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
    let target = app(&address.to_string(), 1, 100);

    // A record in the shape an earlier provider wrote names no target.
    std::fs::create_dir_all(record.parent().unwrap_or_else(|| directory.path()))?;
    std::fs::write(&record, serde_json::to_vec(&definitions)?)?;
    assert_eq!(commands.reconcile(&target).await?, Registration::Registered);
    assert_eq!(commands.reconcile(&target).await?, Registration::Unchanged);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    for other in [
        app(&address.to_string(), 1, 101),
        app(&address.to_string(), 2, 100),
    ] {
        assert_eq!(commands.reconcile(&other).await?, Registration::Registered);
    }
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_confirmed_module_activates_even_when_recording_fails() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["old"], "old").await?;
    std::fs::write(root.join("new.wasm"), variant(&["new"], "new")?)?;
    let blocked = files(root).record;
    let router = Router::new().route(
        "/api/v10/applications/{application}/guilds/{guild}/commands",
        put(move || {
            let blocked = blocked.clone();
            async move {
                // The remote mutation succeeded; the local destination
                // becomes unwritable before its confirmation is recorded.
                std::fs::create_dir(&blocked).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                Ok::<_, StatusCode>(axum::Json(Vec::<Value>::new()))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target = app(&listener.local_addr()?.to_string(), 1, 100);
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let change = commands
        .apply(custom(root, "new.wasm"), Some(&target))
        .await?;
    assert_eq!(change.discord, Registration::Registered);
    assert_eq!(reply(&commands).await?, "new");
    assert!(!commands.registered(&target, &commands.current().definitions));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn checking_applying_and_starting_share_one_size_limit() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    let base = variant(&["model"], "padded")?;
    let at_limit = padded(base.clone(), MODULE_LIMIT)?;
    let over = padded(base, MODULE_LIMIT + 1)?;
    let declared = custom(root, "custom.wasm");

    let refusal = super::super::check(&over)
        .await
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(
        refusal.contains(&format!("more than {MODULE_LIMIT} bytes")),
        "{refusal}"
    );
    std::fs::write(root.join("custom.wasm"), &over)?;
    assert!(matches!(
        commands.apply(declared.clone(), None).await,
        Err(ChangeError::Unusable(_))
    ));
    let unstarted = Module::load(files(root), declared.clone())
        .await
        .err()
        .context("an oversized module started")?;
    assert!(
        format!("{unstarted:#}").contains(&format!("more than {MODULE_LIMIT} bytes")),
        "{unstarted:#}"
    );
    assert_eq!(reply(&commands).await?, "release");

    assert_eq!(super::super::check(&at_limit).await?, ["model"]);
    std::fs::write(root.join("custom.wasm"), &at_limit)?;
    commands.apply(declared.clone(), None).await?;
    assert_eq!(reply(&commands).await?, "padded");
    assert_eq!(
        Module::load(files(root), declared)
            .await?
            .serving()
            .commands,
        ["model"]
    );
    Ok(())
}

/// Run under the persistence suite's syscall fault injection: the first
/// registration creates the state directory for its record, and a repeat
/// must sync that directory and its ancestors again rather than trust
/// that it exists.
#[tokio::test]
async fn a_new_state_directory_is_synced_again_by_a_repeat() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    assert!(!root.join("state").exists());
    let (address, server) = registration_server(Arc::new(AtomicU16::new(200))).await?;
    let target = app(&address.to_string(), 1, 100);
    std::fs::write(
        root.join("custom.wasm"),
        variant(&["model", "other"], "custom")?,
    )?;
    let declared = custom(root, "custom.wasm");
    let first = match commands.apply(declared.clone(), Some(&target)).await {
        Ok(change) => format!("{:?}", change.discord),
        Err(ChangeError::Record(error)) => format!("refused: record ({error})"),
        Err(error) => anyhow::bail!("unexpected refusal: {error}"),
    };
    eprintln!("first change: {first}");
    commands.apply(declared, Some(&target)).await?;
    assert!(commands.registered(&target, &commands.current().definitions));
    assert_eq!(reply(&commands).await?, "custom");
    eprintln!("repeat: registered");
    server.abort();
    Ok(())
}

/// Run under the persistence suite's syscall fault injection: a change
/// clears the record durably before it registers, reports where that
/// stopped, and a repeat must register.
#[tokio::test]
async fn clearing_the_record_reports_its_boundary_and_a_repeat_registers() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let commands = release_commands(root, &["model"], "release").await?;
    // An existing state directory: the first sync of it clears the record.
    std::fs::create_dir(root.join("state"))?;
    let (address, server, requests) =
        counting_registration_server(Arc::new(AtomicU16::new(200))).await?;
    let target = app(&address.to_string(), 1, 100);
    std::fs::write(
        root.join("custom.wasm"),
        variant(&["model", "other"], "custom")?,
    )?;
    let declared = custom(root, "custom.wasm");
    let first = match commands.apply(declared.clone(), Some(&target)).await {
        Ok(change) => format!("{:?}", change.discord),
        Err(ChangeError::Record(error)) => format!("refused: record ({error})"),
        Err(error) => anyhow::bail!("unexpected refusal: {error}"),
    };
    eprintln!(
        "first change {first}; Discord requests {}",
        requests.load(Ordering::SeqCst)
    );
    commands.apply(declared, Some(&target)).await?;
    assert!(commands.registered(&target, &commands.current().definitions));
    assert_eq!(reply(&commands).await?, "custom");
    eprintln!("repeat: registered");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn model_calls_preserve_the_conditional_write() -> anyhow::Result<()> {
    use axum::Json;
    use carillon_discord_module::contract::{HostReply, HostRequest};
    use carillon_protocol::model::{ModelQuery, ModelUpdate};
    use carillon_protocol::{model::ModelScope as Scope, models::Selection};
    let selection = Selection {
        provider: "fixture".to_owned(),
        model: "A".to_owned(),
        options: std::collections::BTreeMap::new(),
    };
    let expected = selection.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let router = Router::new().route(
        "/v1/model",
        put(move |Json(update): Json<ModelUpdate>| {
            let expected = expected.clone();
            async move {
                assert_eq!(update.expected, Some(expected));
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": "changed while applying"})),
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let calls = super::super::InteractionCalls::new(
        crate::client::test_client(format!("http://{address}"))?,
        ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: None,
        },
    );
    let reply = calls
        .answer(
            &serde_json::to_vec(&HostRequest::SetModel {
                scope: Scope::Agent,
                selection: Some(selection.clone()),
                expected: Some(selection),
            })?,
            calls_until(),
        )
        .await;
    assert_eq!(reply, HostReply::Error("changed while applying".to_owned()));
    server.abort();
    Ok(())
}
