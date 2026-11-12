use anyhow::Context as _;

use super::*;
use crate::application::setup::{ask, init};

fn ids(config_path: &Path) -> anyhow::Result<Vec<String>> {
    let providers = LoadedConfig::load(config_path)?.value.providers;
    Ok(providers.into_iter().map(|provider| provider.id).collect())
}

#[test]
fn init_creates_an_installation_with_the_interfaces_named_and_refuses_none() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let workspace = Some(directory.path());
    let refusal = init(&home, workspace, 44_000, &[])
        .err()
        .map(|error| error.to_string());
    assert_eq!(
        refusal.as_deref(),
        Some("an installation needs an interface; name one with --interface")
    );
    assert!(!home.exists());

    let config_path = init(&home, workspace, 44_000, &[Bundled::Web])?;
    assert_eq!(ids(&config_path)?, ["web", "runtime-pi"]);
    let written = std::fs::read(&config_path)?;
    init(&home, None, 44_000, &[Bundled::Web])?;
    assert_eq!(std::fs::read(&config_path)?, written);
    init(&home, None, 44_000, &[Bundled::Discord])?;
    assert_eq!(ids(&config_path)?, ["web", "runtime-pi", "discord-private"]);
    Ok(())
}

#[test]
fn setup_asks_which_interfaces_until_one_is_chosen_with_the_web_chat_preselected()
-> anyhow::Result<()> {
    let mut questions = Shown::answering(&["", "preselected", "0,1"]);
    assert_eq!(
        ask(ProviderRole::Interface, &mut questions)?,
        [Bundled::Web]
    );
    let offered = [
        "A web chat, in your browser on this machine",
        "Discord, as a bot in a server of yours",
        "Where should people talk to Carillon?",
    ];
    assert_eq!(
        questions.transcript,
        [&offered[..], &["Choose at least one."], &offered[..]].concat()
    );
    assert_eq!(
        ask(ProviderRole::Interface, &mut questions)?,
        [Bundled::Web, Bundled::Discord]
    );
    Ok(())
}

#[test]
fn each_enabled_interface_is_set_up_by_its_own_program_with_its_own_id() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let config_path = initialized(&home, directory.path(), Bundled::Pi)?;
    let mut config = Config::read(&config_path)?;
    let mut chat = config.providers[0].clone();
    "chat".clone_into(&mut chat.id);
    "example".clone_into(&mut chat.program);
    "9.9.9".clone_into(&mut chat.version);
    chat.url = "http://127.0.0.1:44009".to_owned();
    let mut unused = config.providers[0].clone();
    "discord-old".clone_into(&mut unused.id);
    unused.url = "http://127.0.0.1:44008".to_owned();
    unused.enabled = false;
    config.providers.extend([chat, unused]);
    config.write(&config_path)?;
    for id in ["chat", "discord-old"] {
        std::fs::write(home.join(format!("credentials/{id}.token")), "unused")?;
    }
    added_pi::fake_providers(directory.path(), &["discord", "example"])?;

    let output = added_pi::run_child(added_pi::CHILD, directory.path(), &home, &[])?;
    assert!(output.status.success(), "{output:?}");
    for (program, arguments) in [
        ("discord", "setup --provider discord-private\n"),
        ("example", "setup --provider chat\n"),
    ] {
        assert_eq!(
            std::fs::read_to_string(home.join(format!("{program}-calls")))?,
            arguments
        );
    }
    let config = LoadedConfig::load(&config_path)?;
    let chat = config.provider("chat", ProviderRole::Interface)?;
    assert_eq!(chat.version, "9.9.9");
    assert!(
        !config
            .provider("discord-old", ProviderRole::Interface)?
            .enabled
    );
    Ok(())
}

const ADD: &str = "application::setup::tests::interfaces::setup_adding_an_interface_child";

#[test]
#[ignore = "run by the test below with isolated provider programs"]
fn setup_adding_an_interface_child() -> anyhow::Result<()> {
    let home = std::env::var_os("CARILLON_TEST_SETUP_HOME").context("setup home")?;
    let name = std::env::var("CARILLON_TEST_SETUP_INTERFACE")?;
    let interface = Bundled::named(ProviderRole::Interface, &name)?;
    let named = Options {
        interfaces: vec![interface],
        ..options()
    };
    setup(Path::new(&home), named, &mut Unasked)
}

#[test]
fn naming_an_interface_adds_it_and_touches_nothing_else_of_an_installation() -> anyhow::Result<()> {
    let cases = [
        (
            Bundled::Discord,
            Bundled::Web,
            "url = \"http://127.0.0.1:44004\"",
            "max_upload_mib = 1024\ndelivery = \"retained\"\n",
        ),
        (
            Bundled::Web,
            Bundled::Discord,
            "url = \"http://127.0.0.1:44001\"",
            "max_upload_mib = 20\n",
        ),
    ];
    for (has, adds, url, facts) in cases {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        let agents = vec![assistant(directory.path())];
        let config_path = initialize(&home, 44_000, &[has, Bundled::Pi], agents)?;
        let refused_by_a_full_walk = "runtime-pi";
        set_provider(&config_path, refused_by_a_full_walk, false)?;
        let before = std::fs::read_to_string(&config_path)?;
        added_pi::fake_providers(directory.path(), &["discord", "web"])?;

        let named = ("CARILLON_TEST_SETUP_INTERFACE", adds.name().as_ref());
        for _ in 0..2 {
            let output = added_pi::run_child(ADD, directory.path(), &home, &[named])?;
            assert!(output.status.success(), "{output:?}");
        }
        let (id, program) = (adds.id(), adds.program());
        let config = LoadedConfig::load(&config_path)?;
        let added = config.provider(id, ProviderRole::Interface)?;
        let (providers, agents) = before.split_at(before.find("\n[[agents]]").context("agents")?);
        assert_eq!(
            std::fs::read_to_string(&config_path)?,
            format!(
                "{providers}\n[[providers]]\nid = \"{id}\"\nrole = \"interface\"\n\
                 program = \"{program}\"\nversion = \"{}\"\n{url}\ntoken_hash = \"{}\"\n\
                 enabled = true\n{facts}{agents}",
                added.version, added.token_hash
            )
        );
        let call = format!("setup --provider {id}\n");
        assert_eq!(
            std::fs::read_to_string(home.join(format!("{program}-calls")))?,
            call.repeat(2)
        );
        assert!(!home.join(format!("{}-calls", has.program())).exists());
        assert!(!home.join("pi-calls").exists());
    }
    Ok(())
}
