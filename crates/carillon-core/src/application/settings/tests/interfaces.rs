use std::time::Duration;

use tokio::net::TcpListener;

use super::*;

const DISCORD: &str = "discord-private";

fn with_web(config: &mut LoadedConfig, listener: &TcpListener) -> Result<()> {
    let mut web = config.provider(DISCORD, ProviderRole::Interface)?.clone();
    "web".clone_into(&mut web.id);
    web.url = format!("http://{}", listener.local_addr()?);
    config.value.providers.push(web);
    Ok(())
}

async fn named(config: &LoadedConfig, interface: &str, arguments: &[&str]) -> Result<String> {
    let named = Named::Interface(Some(interface.to_owned()));
    settings(config, named, &words(arguments)).await
}

fn failure(result: Result<String>) -> String {
    result
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default()
}

#[tokio::test]
async fn a_change_reaches_the_named_interface_and_no_other() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut config, stub, served) = installation(directory.path()).await?;
    // A bound socket accepts connections before anything serves them, so a
    // request to it would show up below.
    let web = TcpListener::bind("127.0.0.1:0").await?;
    with_web(&mut config, &web)?;
    assert_eq!(
        named(&config, DISCORD, &["300", "thinking=high"]).await?,
        "#home\n\
         Agent     assistant\n\
         Listens   always\n\
         Replies   threads\n\
         Model     GPT-6.1 Sol · default\n\
         Thinking  High\n"
    );
    assert_eq!(calls(&stub).len(), 1);
    let contacted = tokio::time::timeout(Duration::from_millis(100), web.accept()).await;
    let contacted = contacted.map(|_| "the other interface was contacted");
    assert_eq!(
        contacted.map_err(|elapsed| elapsed.to_string()).err(),
        Some("deadline has elapsed".to_owned())
    );
    served.abort();
    Ok(())
}

#[tokio::test]
async fn a_channel_needs_its_interface_named_once_two_are_enabled() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut config, stub, served) = installation(directory.path()).await?;
    assert_eq!(run(&config, None, &["300"]).await?, render(&home(0)));
    let web = TcpListener::bind("127.0.0.1:0").await?;
    with_web(&mut config, &web)?;
    assert_eq!(
        failure(run(&config, None, &["300", "thinking=high"]).await),
        "name the channel's interface with --interface, one of: discord-private, web"
    );
    assert_eq!(
        failure(named(&config, "slack", &["300"]).await),
        "slack is not an enabled interface of: discord-private, web"
    );
    assert_eq!(calls(&stub), Vec::<serde_json::Value>::new());
    served.abort();
    Ok(())
}

#[tokio::test]
async fn a_change_drawn_from_a_page_since_changed_is_refused() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, stub, served) = installation(directory.path()).await?;
    lock(&stub).drifts = true;
    assert_eq!(
        failure(named(&config, DISCORD, &["300", "thinking=high"]).await),
        "thinking: Thinking changed meanwhile."
    );
    served.abort();
    Ok(())
}

#[tokio::test]
async fn a_channel_id_with_slashes_is_one_segment_of_the_address() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (config, stub, served) = installation(directory.path()).await?;
    "chat/assistant/7".clone_into(&mut lock(&stub).channel);
    assert_eq!(
        named(&config, DISCORD, &["chat/assistant/7"]).await?,
        render(&home(0))
    );
    served.abort();
    Ok(())
}

#[tokio::test]
async fn channels_are_listed_under_their_interface_and_a_silent_one_says_so() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut config, _stub, served) = installation(directory.path()).await?;
    let web = TcpListener::bind("127.0.0.1:0").await?;
    with_web(&mut config, &web)?;
    let address = web.local_addr()?;
    // Nothing listens there any more, so the request is refused at once.
    drop(web);
    let unanswered =
        format!("web did not list its channels: http://{address}/v1/channels did not answer");
    let listed = run(&config, None, &[]).await?;
    let (answered, silent) = listed.split_at(listed.find("web\n").context("web is not listed")?);
    assert_eq!(
        answered,
        "discord-private\n  300  #home  assistant, always, threads, GPT-6.1 Sol, Medium\n"
    );
    assert!(
        silent.starts_with(&format!("web\n  ⚠ {unanswered}")),
        "{silent}"
    );
    assert!(
        failure(named(&config, "web", &[]).await).starts_with(&unanswered),
        "one silent interface listed"
    );
    served.abort();
    Ok(())
}

#[tokio::test]
async fn an_interface_with_no_channel_pages_says_so_under_its_name() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut config, _stub, served) = installation(directory.path()).await?;
    let web = TcpListener::bind("127.0.0.1:0").await?;
    with_web(&mut config, &web)?;
    let pages = Router::new().route("/v1/channels", get(|| async { Json(Vec::<Page>::new()) }));
    let web = tokio::spawn(async move { axum::serve(web, pages).await });
    assert_eq!(
        named(&config, "web", &[]).await?,
        "web\n  no channels with settings of their own\n"
    );
    assert!(
        run(&config, None, &[])
            .await?
            .ends_with("Sol, Medium\nweb\n  no channels with settings of their own\n")
    );
    web.abort();
    served.abort();
    Ok(())
}
