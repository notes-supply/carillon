use super::*;

#[tokio::test]
async fn agents_of_the_same_name_are_routed_and_configured_by_distinct_labels() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let config = with_two_agents(directory.path())?;
    let mut registry = config.value.clone();
    for agent in &mut registry.agents {
        agent.name = "T3 Code · Notes".to_owned();
    }
    std::fs::write(&config.path, toml::to_string(&registry)?)?;
    let config = config.reload()?;
    let address = discord(Arc::new(Discord::default())).await?;
    let (result, shown) = run(
        &config,
        address,
        &[TOKEN, "", "1 2", "2", "1", "2", "1", "", ""],
    )
    .await;
    result?;
    assert!(
        shown.contains("T3 Code · Notes (default)\nT3 Code · Notes (code)\nWho answers in #agent?"),
        "{shown}"
    );
    for id in ["default", "code"] {
        assert!(
            shown.contains(&format!("When should T3 Code · Notes ({id}) answer?")),
            "{shown}"
        );
    }
    assert!(
        shown.contains(
            "#general (300) (T3 Code · Notes (code)), #agent (T3 Code · Notes (default))"
        ),
        "{shown}"
    );
    let routes = DiscordConfig::load(&config, "discord-private")?.routes;
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].channel_id, "300");
    assert_eq!(routes[0].agent, "code");
    assert_eq!(routes[0].conversation, DiscordConversation::Channel);
    assert_eq!(routes[1].channel_id, "301");
    assert_eq!(routes[1].agent, "default");
    assert_eq!(routes[1].conversation, DiscordConversation::Thread);
    Ok(())
}

#[tokio::test]
async fn servers_and_channels_of_the_same_name_are_told_apart_by_id() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord::default())).await?;
    // The second Notes, its second #general, every message in its own
    // thread, and the application's owner, who is not in that server, then
    // someone who is.
    let (result, shown) = run(
        &config,
        address,
        &[TOKEN, "3", "3", "", "", "", "none", "250"],
    )
    .await;
    result?;
    assert!(
        shown.contains("owner (200) is not a member of Notes (102)."),
        "{shown}"
    );
    assert!(
        shown.contains("Notes (100)\nOther\nNotes (102)\n"),
        "{shown}"
    );
    assert!(
        shown.contains("#general (300)\n#agent\n#general (303)\n"),
        "{shown}"
    );
    let settings = DiscordConfig::load(&config, "discord-private")?;
    assert_eq!(settings.guild_id, "102");
    assert_eq!(settings.routes[0].channel_id, "303");
    Ok(())
}

#[tokio::test]
async fn a_route_is_saved_only_once_the_bot_may_answer_there_as_its_mode_needs()
-> anyhow::Result<()> {
    let without_role = || Discord {
        unroled: AtomicUsize::new(usize::MAX),
        ..Discord::default()
    };

    // Without its role, the bot cannot see #agent; answering there ends
    // setup unanswered, with nothing written.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(without_role())).await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "2", "2"]).await;
    assert_eq!(
        result.map_err(|error| error.to_string()),
        Err("no answer to Press Enter to check again.".to_owned())
    );
    assert!(
        shown.contains(
            "The bot cannot answer in #agent: allow it View Channel in the channel's settings."
        ),
        "{shown}"
    );
    assert!(bot_tokens(&config)?.is_empty());
    assert!(!config.provider_config_path("discord-private")?.exists());

    // #general needs no role for the channel as one conversation, and lacking
    // cards and reactions there is only said.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(without_role())).await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1", "2", "", ""]).await;
    result?;
    assert!(!shown.contains("cannot answer"), "{shown}");
    assert!(
        shown.contains(
            "In #general (300) the bot lacks Embed Links, Add Reactions, so the agent answers \
             there without cards or reactions."
        ),
        "{shown}"
    );
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.routes[0].channel_id,
        "300"
    );

    // A thread per conversation needs the role in both; once it is granted,
    // the next check passes and both are saved.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord {
        unroled: AtomicUsize::new(2),
        ..Discord::default()
    }))
    .await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1 2", "1", "", "", "", ""]).await;
    result?;
    assert!(
        shown.contains(
            "The bot cannot answer in #general (300): allow it Create Public Threads, Send \
             Messages in Threads in the channel's settings.\nThe bot cannot answer in #agent: \
             allow it View Channel, Create Public Threads, Send Messages in Threads in the \
             channel's settings.\nPress Enter to check again."
        ),
        "{shown}"
    );
    assert_eq!(shown.matches("Press Enter to check again.").count(), 2);
    let routes = DiscordConfig::load(&config, "discord-private")?.routes;
    assert_eq!(
        routes
            .iter()
            .map(|route| route.channel_id.as_str())
            .collect::<Vec<_>>(),
        ["300", "301"]
    );

    // An administrator role passes every overwrite.
    let directory = tempfile::tempdir()?;
    let config = fresh(directory.path())?;
    let address = discord(Arc::new(Discord {
        admin: true,
        ..Discord::default()
    }))
    .await?;
    let (result, shown) = run(&config, address, &[TOKEN, "1", "1 2", "1", "", ""]).await;
    result?;
    assert!(
        !shown.contains("lacks") && !shown.contains("cannot answer"),
        "{shown}"
    );
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?
            .routes
            .len(),
        2
    );
    Ok(())
}

/// With several agents, setup asks who answers in each channel, then how
/// each answering agent answers, once; each route gets its agent and that
/// agent's mode, and a channel needs the permissions of its own mode.
#[tokio::test]
async fn each_channel_is_routed_to_the_agent_chosen_for_it_with_that_agents_mode()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config = with_two_agents(directory.path())?;
    // The bot has its role only from the second look at it.
    let address = discord(Arc::new(Discord {
        unroled: AtomicUsize::new(1),
        ..Discord::default()
    }))
    .await?;
    let (result, shown) = run(
        &config,
        address,
        &[
            TOKEN, "",      // Notes
            "1 2 3", // every channel;
            "1",     // the test agent answers in #general (300),
            "2",     //   T3 Code in #agent,
            "1",     //   the test agent in #general (303);
            "",      // the test agent every message, each in its own thread,
            "2",     //   T3 Code every message, the channel as one conversation;
            "",      // the role, granted meanwhile, is checked again;
            "",      // the application's owner,
            "",      //   and nobody else
        ],
    )
    .await;
    result?;
    assert!(
        shown.contains(
            "Test agent\nT3 Code · Notes\nWho answers in #general (300)?\n\
             Test agent\nT3 Code · Notes\nWho answers in #agent?\n\
             Test agent\nT3 Code · Notes\nWho answers in #general (303)?\n"
        ),
        "{shown}"
    );
    assert_eq!(shown.matches("When should Test agent answer?").count(), 1);
    assert_eq!(
        shown.matches("When should T3 Code · Notes answer?").count(),
        1
    );
    // The thread permissions are asked only where conversations are threads.
    assert!(
        shown.contains(
            "The bot cannot answer in #general (300): allow it Create Public Threads, Send \
             Messages in Threads in the channel's settings.\nThe bot cannot answer in #agent: \
             allow it View Channel in the channel's settings.\nThe bot cannot answer in \
             #general (303): allow it Create Public Threads, Send Messages in Threads in the \
             channel's settings.\nPress Enter to check again.\n"
        ),
        "{shown}"
    );
    assert!(
        shown.contains(
            "carillon-bot answers owner in #general (300) (Test agent), #agent (T3 Code · \
             Notes), #general (303) (Test agent) on Notes (100)."
        ),
        "{shown}"
    );
    let route = |channel: &str, agent: &str, conversation| DiscordRoute {
        channel_id: channel.to_owned(),
        agent: agent.to_owned(),
        activation: DiscordActivation::Always,
        conversation,
    };
    assert_eq!(
        DiscordConfig::load(&config, "discord-private")?.routes,
        [
            route("300", "default", DiscordConversation::Thread),
            route("301", "code", DiscordConversation::Channel),
            route("303", "default", DiscordConversation::Thread),
        ]
    );
    Ok(())
}
