use super::{
    Assistant, Config, LoadedConfig, Options, Runtime, Shown, Unasked, fake_t3, initialize, model,
    options, set_up_t3,
};
use crate::application::{
    admin::set_agents,
    setup::{agents, runtimes, sign_in_to_pi},
};

#[test]
fn a_t3_assistant_moves_to_pi_by_resources_and_keeps_its_paths() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let path = initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let mut config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    config.agents[0].id = "my-assistant".to_owned();
    config.agents[0].name = "My assistant".to_owned();
    config.agents[0].workspace = "..".into();
    set_agents(&path, config.agents)?;
    let offer = serde_json::json!({
        "projects": [{"id": "notes", "title": "Notes", "workspace": directory.path()}],
        "models": [model("codex", "gpt", "GPT")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    set_up_t3(
        &home,
        &options(),
        &Assistant::Pi,
        &program,
        &mut Shown::answering(&["", "token", "0", "0"]),
    )?;
    let stored: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    let assistant = stored.agent("my-assistant")?;
    assert_eq!(assistant.name, "My assistant");
    assert_eq!(assistant.runtime, "runtime-pi");
    assert_eq!(assistant.workspace, std::path::Path::new(".."));
    assert_eq!(
        assistant.resources.as_deref(),
        Some(std::path::Path::new("agents/default"))
    );
    assert!(
        stored
            .provider("runtime-pi", super::ProviderRole::Runtime)?
            .enabled
    );
    assert_eq!(stored.agents.len(), 2);
    Ok(())
}

#[test]
fn a_pi_assistant_moves_to_t3_by_resources_and_none_drops_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let path = initialize(&home, directory.path(), 44_000, Runtime::Pi)?;
    let mut config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    config.agents[0].id = "my-assistant".to_owned();
    set_agents(&path, config.agents)?;
    let project = super::super::Project {
        id: "notes".to_owned(),
        title: "Notes".to_owned(),
        workspace: directory.path().to_path_buf(),
    };
    agents(&home, &options(), Assistant::T3(&project), &[&project])?;
    let loaded = LoadedConfig::load(&path)?;
    assert_eq!(loaded.value.agent("my-assistant")?.runtime, "runtime-t3");
    assert_eq!(
        loaded.value.agent("my-assistant")?.workspace,
        project.workspace
    );
    sign_in_to_pi(&home, true, &mut Unasked)?;
    agents(&home, &options(), Assistant::None, &[&project])?;
    let loaded = LoadedConfig::load(&path)?;
    assert_eq!(loaded.value.agents.len(), 1);
    assert_eq!(loaded.value.agents[0].id, "t3-notes");
    assert_eq!(loaded.value.agents[0].resources, None);
    Ok(())
}

#[test]
fn a_pi_assistant_is_created_when_a_code_only_installation_chooses_pi() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let path = initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let mut config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    config.agents[0].resources = None;
    config.agents[0].id = "t3-code".to_owned();
    set_agents(&path, config.agents)?;
    let options = Options {
        workspace: Some(directory.path().to_path_buf()),
        ..options()
    };
    agents(&home, &options, Assistant::Pi, &[])?;
    let stored: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(stored.agents.len(), 1);
    assert_eq!(stored.agent("assistant")?.runtime, "runtime-pi");
    assert_eq!(
        stored.agent("assistant")?.resources.as_deref(),
        Some(std::path::Path::new("agents/default"))
    );
    Ok(())
}

#[test]
fn an_explicit_runtime_extends_a_pi_installation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::Pi)?;
    let options = Options {
        runtimes: vec![Runtime::T3],
        ..options()
    };
    assert_eq!(runtimes(&home, &options)?, [Runtime::Pi, Runtime::T3]);
    let project = super::super::Project {
        id: "notes".to_owned(),
        title: "Notes".to_owned(),
        workspace: directory.path().to_path_buf(),
    };
    agents(&home, &options, Assistant::Pi, &[&project])?;
    assert_eq!(
        super::registered_runtimes(&home)?,
        [Runtime::Pi, Runtime::T3]
    );
    let stored: Config = toml::from_str(&std::fs::read_to_string(home.join("carillon.toml"))?)?;
    assert_eq!(stored.agent("assistant")?.runtime, "runtime-pi");
    assert_eq!(stored.agent("t3-notes")?.runtime, "runtime-t3");
    Ok(())
}

#[test]
fn t3_reruns_use_the_runtimes_agent_offer_including_older_profiles() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let offer = serde_json::json!({
        "projects": [{"id": "notes", "title": "Notes", "workspace": directory.path()}],
        "models": [model("carillon_codex", "gpt", "Profile GPT"), model("codex", "gpt", "GPT")],
        "agentModels": {"t3-notes": [model("codex", "gpt", "GPT")]},
    });
    let program = fake_t3(directory.path(), &offer)?;
    set_up_t3(
        &home,
        &options(),
        &Assistant::None,
        &program,
        &mut Shown::answering(&["", "token", "0", "0"]),
    )?;
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    assert!(
        calls.contains("--agent t3-notes --project notes --model codex/gpt"),
        "{calls}"
    );
    Ok(())
}
