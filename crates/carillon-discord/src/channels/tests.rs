use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context as _;
use axum::{Json, Router, http::StatusCode, routing::post};
use carillon_discord_module::contract::Notice;
use carillon_protocol::settings::{
    Change, Revision, RuntimePart, RuntimeSettings, Setting, SettingsRequest, Value,
};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;

use super::{Routing, change, page, settled};
use crate::{
    client::test_client,
    config::{DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute, Writer},
};

fn route(agent: &str) -> DiscordRoute {
    DiscordRoute {
        channel_id: "300".to_owned(),
        agent: agent.to_owned(),
        activation: DiscordActivation::Mention,
        conversation: DiscordConversation::Channel,
    }
}

/// A core that answers every settings request with no lines after `delay`,
/// keeping each request.
async fn core(
    delay: Duration,
) -> anyhow::Result<(
    crate::client::CoreClient,
    Arc<Mutex<Vec<SettingsRequest>>>,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let kept = requests.clone();
    let router = Router::new().route(
        "/v1/settings",
        post(move |Json(request): Json<SettingsRequest>| {
            let kept = kept.clone();
            async move {
                kept.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request);
                tokio::time::sleep(delay).await;
                Json(serde_json::json!({"settings": []}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((test_client(format!("http://{address}"))?, requests, server))
}

fn taken(requests: &Mutex<Vec<SettingsRequest>>) -> Vec<SettingsRequest> {
    std::mem::take(
        &mut *requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn until() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

fn change_of(setting: &Setting, value: &str) -> Change {
    Change {
        setting: setting.id.clone(),
        value: Some(Value::One {
            id: value.to_owned(),
        }),
        expected: setting.revision.clone(),
    }
}

fn line<'a>(page: &'a carillon_protocol::settings::Page, id: &str) -> anyhow::Result<&'a Setting> {
    page.interface
        .iter()
        .find(|line| line.id == id)
        .ok_or_else(|| anyhow::anyhow!("no {id} line"))
}

fn file(writer: &Writer) -> anyhow::Result<DiscordConfig> {
    DiscordConfig::read(writer.installation(), "discord-private")
}

fn served(writer: &Writer) -> Routing {
    Routing::of(&writer.current(), "300")
}

#[tokio::test]
async fn route_lines_offer_their_choices_and_are_revised_for_their_route() -> anyhow::Result<()> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let routed = Routing::Routed(route("default"));
    let drawn = page(&core, &writer, &routed, "<#300>".to_owned(), until()).await;
    let lines: Vec<_> = drawn
        .interface
        .iter()
        .map(|line| {
            (
                line.id.as_str(),
                line.value_name(),
                line.choices
                    .iter()
                    .map(|choice| (choice.id.as_str(), choice.name.as_str()))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            (
                "agent",
                Some("default".to_owned()),
                vec![("default", "default"), ("other", "other")]
            ),
            (
                "listens",
                Some("mentions".to_owned()),
                vec![("mention", "mentions"), ("always", "always")]
            ),
            (
                "replies",
                Some("channel".to_owned()),
                vec![("thread", "threads"), ("channel", "channel")]
            ),
        ]
    );
    assert_eq!(
        drawn.runtime,
        RuntimePart::Settings(RuntimeSettings::default())
    );
    let canonical = r#"["listens",["channel","discord","300","default"],"mention"]"#;
    assert_eq!(
        drawn.interface[1].revision,
        Revision::try_from(hex::encode(Sha256::digest(canonical))).map_err(anyhow::Error::msg)?
    );
    let rerouted = page(
        &core,
        &writer,
        &Routing::Routed(route("other")),
        "<#300>".to_owned(),
        until(),
    )
    .await;
    assert_ne!(
        rerouted.interface[1].revision, drawn.interface[1].revision,
        "a re-routed channel's lines are other lines"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_route_change_is_written_served_and_reaches_no_runtime_change() -> anyhow::Result<()> {
    let (core, requests, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    taken(&requests);
    let shown = change(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "listens")?, "mention"),
        until(),
    )
    .await
    .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    assert_eq!(
        line(&shown, "listens")?.value_name().as_deref(),
        Some("mentions")
    );
    assert_eq!(
        file(&writer)?.routes[0].activation,
        DiscordActivation::Mention
    );
    assert_eq!(
        writer.current().routes[0].activation,
        DiscordActivation::Mention
    );
    assert!(
        taken(&requests)
            .iter()
            .all(|request| request.change.is_none()),
        "the runtime was only read"
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_stale_route_line_is_refused_and_nothing_is_written() -> anyhow::Result<()> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    change(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "replies")?, "thread"),
        until(),
    )
    .await
    .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    let before = fs::read(
        writer
            .installation()
            .provider_config_path("discord-private")?,
    )?;
    let (shown, notice) = settled(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "replies")?, "channel"),
        until(),
    )
    .await;
    assert_eq!(notice, Some(Notice::Stale));
    assert_eq!(
        line(&shown, "replies")?.value_name().as_deref(),
        Some("threads")
    );
    assert_eq!(
        fs::read(
            writer
                .installation()
                .provider_config_path("discord-private")?
        )?,
        before
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_re_route_moves_the_page_to_the_new_agent_and_stales_the_old_card() -> anyhow::Result<()>
{
    let (core, requests, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    let shown = change(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "agent")?, "other"),
        until(),
    )
    .await
    .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    assert_eq!(
        shown
            .subject
            .as_ref()
            .map(carillon_protocol::settings::Subject::agent_id),
        Some("other")
    );
    let read = taken(&requests);
    assert_eq!(
        read.last().map(|request| request.subject.agent_id()),
        Some("other"),
        "the page reads the new agent's runtime"
    );
    // The old card's Listens line was drawn for the old agent.
    let refusal = change(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "listens")?, "mention"),
        until(),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("a card drawn before the re-route changed the route"))?;
    assert_eq!(refusal.status, StatusCode::CONFLICT);
    assert_eq!(
        file(&writer)?.routes[0].activation,
        DiscordActivation::Always
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn an_unrouted_channel_offers_its_agent_and_reaches_no_runtime_until_routed()
-> anyhow::Result<()> {
    let (core, requests, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let unrouted = Routing::of(&writer.current(), "301");
    let drawn = page(&core, &writer, &unrouted, "<#301>".to_owned(), until()).await;
    assert_eq!(drawn.lines(), ["Agent"]);
    assert_eq!(drawn.subject, None);
    assert!(taken(&requests).is_empty(), "nothing asked a runtime");
    let refusal = change(
        &core,
        &writer,
        &unrouted,
        "<#301>".to_owned(),
        Change {
            setting: "model".to_owned(),
            value: None,
            expected: line(&drawn, "agent")?.revision.clone(),
        },
        until(),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("an unrouted channel changed a runtime line"))?;
    assert_eq!(refusal.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(taken(&requests).is_empty(), "nothing asked a runtime");

    let routed = change(
        &core,
        &writer,
        &unrouted,
        "<#301>".to_owned(),
        change_of(line(&drawn, "agent")?, "other"),
        until(),
    )
    .await
    .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
    assert_eq!(
        routed.lines(),
        ["Agent    other", "Listens  mentions", "Replies  threads"]
    );
    assert_eq!(
        file(&writer)?.routes.last(),
        Some(&DiscordRoute {
            channel_id: "301".to_owned(),
            agent: "other".to_owned(),
            activation: DiscordActivation::Mention,
            conversation: DiscordConversation::Thread,
        })
    );
    let read = taken(&requests);
    assert_eq!(read.len(), 1, "one read, after the route exists");
    assert_eq!(read[0].subject.agent_id(), "other");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_change_unanswered_by_the_deadline_is_unconfirmed_and_not_retried() -> anyhow::Result<()>
{
    let (core, requests, server) = core(Duration::from_secs(30)).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let line = Setting {
        id: "model.thinking".to_owned(),
        name: "Thinking".to_owned(),
        choices: Vec::new(),
        value: None,
        inherited: true,
        revision: Revision::try_from("0".repeat(64)).map_err(anyhow::Error::msg)?,
    };
    let started = Instant::now();
    let (shown, notice) = settled(
        &core,
        &writer,
        &Routing::Routed(route("default")),
        "<#300>".to_owned(),
        change_of(&line, "high"),
        started + Duration::from_millis(300),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(notice, Some(Notice::Unconfirmed));
    assert_eq!(
        taken(&requests)
            .iter()
            .filter(|request| request.change.is_some())
            .count(),
        1
    );
    assert_eq!(
        shown.runtime,
        RuntimePart::Unavailable {
            reason: "The runtime did not answer in time.".to_owned()
        }
    );
    assert_eq!(shown.interface.len(), 3, "the route lines still draw");
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_route_change_held_past_its_deadline_by_a_module_change_is_unconfirmed_and_not_made()
-> anyhow::Result<()> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    let before = fs::read(
        writer
            .installation()
            .provider_config_path("discord-private")?,
    )?;
    let (holding, held) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let module = tokio::spawn({
        let writer = writer.clone();
        async move {
            writer
                .declare(
                    |_| Ok(()),
                    move |_, _| async move {
                        let _ = holding.send(());
                        let _ = released.await;
                        Err(crate::module::ChangeError::Busy("held".into()))
                    },
                )
                .await
                .map(|_| ())
        }
    });
    held.await?;
    let started = Instant::now();
    let (shown, notice) = tokio::time::timeout(
        Duration::from_secs(5),
        settled(
            &core,
            &writer,
            &served(&writer),
            "#300".to_owned(),
            change_of(line(&drawn, "listens")?, "mention"),
            started + Duration::from_millis(500),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the card waited for the module change past its deadline"))?;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(notice, Some(Notice::Unconfirmed));
    assert_eq!(
        line(&shown, "listens")?.value_name().as_deref(),
        Some("always")
    );
    let _ = release.send(());
    let refused = module.await?.err().context("the module change was made")?;
    assert!(
        matches!(
            refused,
            crate::config::Refused::Module(crate::module::ChangeError::Busy(_))
        ),
        "{refused:?}"
    );
    assert_eq!(
        fs::read(
            writer
                .installation()
                .provider_config_path("discord-private")?
        )?,
        before
    );
    assert_eq!(
        writer.current().routes[0].activation,
        DiscordActivation::Always
    );
    server.abort();
    Ok(())
}

fn block_the_executor_for(duration: Duration) {
    std::thread::sleep(duration);
}

#[tokio::test]
async fn a_route_change_whose_writer_frees_only_after_its_deadline_is_unconfirmed_and_not_made()
-> anyhow::Result<()> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    let before = fs::read(
        writer
            .installation()
            .provider_config_path("discord-private")?,
    )?;
    let (holding, held) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let module = tokio::spawn({
        let writer = writer.clone();
        async move {
            writer
                .declare(
                    |_| Ok(()),
                    move |_, _| async move {
                        let _ = holding.send(());
                        let _ = released.await;
                        block_the_executor_for(Duration::from_millis(150));
                        Err(crate::module::ChangeError::Busy("held".into()))
                    },
                )
                .await
                .map(|_| ())
        }
    });
    held.await?;
    let deadline = Instant::now() + Duration::from_millis(100);
    let change = change_of(line(&drawn, "listens")?, "mention");
    let (starting, started) = tokio::sync::oneshot::channel();
    let card = tokio::spawn({
        let writer = writer.clone();
        async move {
            let routing = served(&writer);
            let _ = starting.send(());
            settled(
                &core,
                &writer,
                &routing,
                "#300".to_owned(),
                change,
                deadline,
            )
            .await
        }
    });
    started.await?;
    let _ = release.send(());
    let refused = module.await?.err().context("the module change was made")?;
    assert!(
        matches!(
            refused,
            crate::config::Refused::Module(crate::module::ChangeError::Busy(_))
        ),
        "{refused:?}"
    );
    let (shown, notice) = card.await?;
    assert_eq!(notice, Some(Notice::Unconfirmed));
    assert_eq!(
        line(&shown, "listens")?.value_name().as_deref(),
        Some("always")
    );
    assert_eq!(
        writer.current().routes[0].activation,
        DiscordActivation::Always
    );
    assert_eq!(
        fs::read(
            writer
                .installation()
                .provider_config_path("discord-private")?
        )?,
        before
    );
    server.abort();
    Ok(())
}

#[tokio::test]
async fn a_route_change_in_place_but_not_synced_shows_and_serves_it_unconfirmed()
-> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, writer) = crate::config::tests::writer()?;
    let drawn = page(&core, &writer, &served(&writer), "#300".to_owned(), until()).await;
    let path = writer
        .installation()
        .provider_config_path("discord-private")?;
    let providers = path.parent().context("settings directory")?;
    // Writable but not readable: the new file is renamed into place, and
    // the directory cannot be opened to sync it.
    fs::set_permissions(providers, fs::Permissions::from_mode(0o300))?;
    let (shown, notice) = settled(
        &core,
        &writer,
        &served(&writer),
        "#300".to_owned(),
        change_of(line(&drawn, "listens")?, "mention"),
        until(),
    )
    .await;
    fs::set_permissions(providers, fs::Permissions::from_mode(0o700))?;
    let Some(Notice::Refused(warning)) = notice else {
        anyhow::bail!("the card showed {notice:?}");
    };
    assert!(warning.contains("not confirmed durable"), "{warning}");
    assert_eq!(
        line(&shown, "listens")?.value_name().as_deref(),
        Some("mentions")
    );
    assert_eq!(file(&writer)?.routes, writer.current().routes);
    assert_eq!(
        writer.current().routes[0].activation,
        DiscordActivation::Mention
    );
    server.abort();
    Ok(())
}

/// The settings file after each of `agents` is chosen on the direct
/// messages' page, in turn, starting from `owned`.
async fn direct_message_agents(owned: &str, agents: &[&str]) -> anyhow::Result<Vec<String>> {
    let (core, _, server) = core(Duration::ZERO).await?;
    let (_installation, generated) = crate::config::tests::writer()?;
    let path = generated
        .installation()
        .provider_config_path("discord-private")?;
    fs::write(&path, owned)?;
    let writer = Writer::new(
        crate::config::Settings::new(DiscordConfig::load(
            generated.installation(),
            "discord-private",
        )?),
        Arc::new(generated.installation().clone()),
        "discord-private",
    );
    let direct = || Routing::of(&writer.current(), "dm");
    let mut files = Vec::new();
    for agent in agents {
        let title = "Direct messages".to_owned();
        let drawn = page(&core, &writer, &direct(), title.clone(), until()).await;
        change(
            &core,
            &writer,
            &direct(),
            title,
            change_of(line(&drawn, "agent")?, agent),
            until(),
        )
        .await
        .map_err(|refusal| anyhow::anyhow!(refusal.message))?;
        files.push(fs::read_to_string(&path)?);
    }
    server.abort();
    Ok(files)
}

#[tokio::test]
async fn the_direct_message_agent_is_written_into_the_owners_file_and_back() -> anyhow::Result<()> {
    let (_installation, writer) = crate::config::tests::writer()?;
    let generated = fs::read_to_string(
        writer
            .installation()
            .provider_config_path("discord-private")?,
    )?;
    let owned = format!(
        "# The owner's settings.\n{}",
        generated.replace("[[routes]]", "# Where the agent answers.\n[[routes]]")
    );
    let routed = |agent: &str| format!("{owned}\n[direct_messages]\nagent = \"{agent}\"\n");
    assert_eq!(
        direct_message_agents(&owned, &["default", "other", "default"]).await?,
        [routed("default"), routed("other"), routed("default")]
    );

    let commented =
        format!("{owned}\n# Who answers me directly.\n[direct_messages]\nagent = 'default' # me\n");
    assert_eq!(
        direct_message_agents(&commented, &["other", "default"]).await?,
        [
            commented.replace("'default' # me", "'other' # me"),
            commented
        ]
    );
    Ok(())
}
