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

    let config_path = init(&home, workspace, 44_000, &[Bundled::Discord])?;
    assert_eq!(ids(&config_path)?, ["discord-private", "runtime-pi"]);
    let written = std::fs::read(&config_path)?;
    init(&home, None, 44_000, &[Bundled::Discord])?;
    assert_eq!(std::fs::read(&config_path)?, written);
    Ok(())
}

#[test]
fn setup_asks_which_interfaces_until_one_is_chosen() -> anyhow::Result<()> {
    let mut questions = Shown::answering(&["", "0"]);
    assert_eq!(
        ask(ProviderRole::Interface, &mut questions)?,
        [Bundled::Discord]
    );
    assert_eq!(
        questions.transcript,
        [
            "Discord, as a bot in a server of yours",
            "Where should people talk to Carillon?",
            "Choose at least one.",
            "Discord, as a bot in a server of yours",
            "Where should people talk to Carillon?",
        ]
    );
    Ok(())
}

#[test]
fn each_interface_is_set_up_by_its_own_program_with_its_own_id() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let config_path = initialized(&home, directory.path(), Bundled::Pi)?;
    let mut config = Config::read(&config_path)?;
    let mut chat = config.providers[0].clone();
    "chat".clone_into(&mut chat.id);
    "example".clone_into(&mut chat.program);
    "9.9.9".clone_into(&mut chat.version);
    chat.url = "http://127.0.0.1:44009".to_owned();
    config.providers.push(chat);
    config.write(&config_path)?;
    std::fs::write(home.join("credentials/chat.token"), "unused")?;
    added_pi::fake_providers(directory.path(), &["discord", "example"])?;

    let output = added_pi::rerun(directory.path(), &home, None)?;
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
    let chat = LoadedConfig::load(&config_path)?;
    let chat = chat.provider("chat", ProviderRole::Interface)?;
    assert_eq!(chat.version, "9.9.9");
    Ok(())
}
