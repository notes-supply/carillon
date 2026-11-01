use super::{
    Config, LoadedConfig, Options, Runtime, Shown, fake_t3, initialized, keep_t3, model, options,
    set_up_t3,
};
use crate::application::{
    admin::set_agents,
    setup::{agents, runtimes},
};

#[test]
fn setting_up_t3_keeps_the_assistants_id_and_paths_on_pi() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let path = initialized(&home, directory.path(), Runtime::Pi)?;
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
        true,
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
    assert_eq!(stored.agent("t3-notes")?.runtime, "runtime-t3");
    assert_eq!(stored.agents.len(), 2);
    Ok(())
}

#[test]
fn a_pi_assistant_is_created_when_a_code_only_installation_adds_pi() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let path = initialized(&home, directory.path(), Runtime::T3)?;
    set_agents(
        &path,
        vec![carillon_installation::config::AgentConfig {
            id: "t3-code".to_owned(),
            name: "T3 Code".to_owned(),
            runtime: Runtime::T3.id().to_owned(),
            workspace: directory.path().to_path_buf(),
            resources: None,
        }],
    )?;
    let options = Options {
        workspace: Some(directory.path().to_path_buf()),
        ..options()
    };
    agents(&home, &options, true, Some(&[]))?;
    let stored: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(stored.agents.len(), 1);
    assert!(!home.join("agents/default/sessions").exists());
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
    initialized(&home, directory.path(), Runtime::Pi)?;
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
    agents(&home, &options, true, Some(&[&project]))?;
    assert_eq!(
        super::registered_runtimes(&home)?,
        [Runtime::Pi, Runtime::T3]
    );
    let stored: Config = toml::from_str(&std::fs::read_to_string(home.join("carillon.toml"))?)?;
    assert_eq!(stored.agent("assistant")?.runtime, "runtime-pi");
    assert_eq!(stored.agent("t3-notes")?.runtime, "runtime-t3");
    let loaded = LoadedConfig::load(&home.join("carillon.toml"))?;
    assert_eq!(loaded.value.agents.len(), 2);
    Ok(())
}

#[test]
fn t3_setup_offers_each_agent_the_models_the_runtime_offers_it() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialized(&home, directory.path(), Runtime::T3)?;
    let offer = serde_json::json!({
        "projects": [{"id": "notes", "title": "Notes", "workspace": directory.path()}],
        "models": [model("other", "gpt", "Other GPT"), model("codex", "gpt", "GPT")],
        "agentModels": {"t3-notes": [model("codex", "gpt", "GPT")]},
    });
    let program = fake_t3(directory.path(), &offer)?;
    set_up_t3(
        &home,
        &options(),
        false,
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

#[test]
fn unreadable_t3_settings_are_replaced_and_declined_ones_rewritten() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialized(&home, directory.path(), Runtime::T3)?;
    let settings = home.join("providers/runtime-t3.toml");
    let token = home.join("credentials/t3.token");
    std::fs::write(&token, "token")?;
    let offer = serde_json::json!({
        "projects": [{"id": "notes", "title": "Notes", "workspace": directory.path()}],
        "models": [model("codex", "gpt", "GPT")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    let calls = directory.path().join("calls");
    let earlier =
        "[agents.t3-notes]\nproject_id = 'notes'\n[agents.t3-notes.model]\nmodel = 'gpt'\n";
    let readable = "[agents.t3-notes]\nproject_id = 'notes'\n";
    for (saved, declined) in [(earlier, false), (readable, true)] {
        std::fs::write(&settings, saved)?;
        let mut questions = Shown::answering(&["", "token", "0", "0"]);
        questions.confirmations.extend(declined.then_some(false));
        assert!(!keep_t3(&home, &program, &mut questions)?);
        assert_eq!(settings.exists(), declined);
        if !declined {
            assert_eq!(
                questions.transcript,
                [format!(
                    "The saved T3 settings cannot be kept, so setup replaces them: check \
                     refuses {}",
                    settings.display()
                )]
            );
        }
        set_up_t3(&home, &options(), false, &program, &mut questions)?;
        let ran = std::fs::read_to_string(&calls)?;
        assert!(
            ran.contains("--agent t3-notes --project notes --model codex/gpt"),
            "{ran}"
        );
        assert_eq!(std::fs::read_to_string(&token)?, "token");
        std::fs::remove_file(&calls)?;
    }
    Ok(())
}
