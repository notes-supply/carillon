use anyhow::Context as _;

use super::*;

fn rerun_projects(selected: &'static str, expected: &[(&str, &str)]) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    std::fs::create_dir_all(&first)?;
    std::fs::create_dir_all(&second)?;
    let offer = serde_json::json!({
        "projects": [
            {"id": "p1", "title": "Notes", "workspace": first},
            {"id": "p2", "title": "Notes", "workspace": second},
            {"id": "p3", "title": "Notes", "workspace": first},
        ],
        "models": [model("codex", "gpt", "GPT")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    set_up_t3(
        &home,
        &options(),
        false,
        &program,
        SavedT3::Replaced,
        &mut Shown::answering(&["", "token", "0,1", "0", "0"]),
    )?;
    std::fs::write(
        home.join("providers/runtime-t3.toml"),
        "[agents.t3-notes]\nproject_id = 'p1'\n[agents.t3-notes-2]\nproject_id = 'p2'\n",
    )?;
    set_up_t3(
        &home,
        &options(),
        false,
        &program,
        SavedT3::Replaced,
        &mut Shown::answering(&["", "token", selected, "0", "0"]),
    )?;
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    let ids: Vec<&str> = config
        .value
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect();
    assert_eq!(ids, expected.iter().map(|(id, _)| *id).collect::<Vec<_>>());
    assert_eq!(
        config
            .value
            .agent("t3-notes")
            .err()
            .context("the former route silently changed projects")?
            .to_string(),
        "agent t3-notes is not configured"
    );
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    for (id, project) in expected {
        assert!(
            calls.contains(&format!(
                "--agent {id} --project {project} --model codex/gpt"
            )),
            "{calls}"
        );
    }
    assert_eq!(config.value.agent("t3-notes-2")?.workspace, second);
    Ok(())
}

#[test]
fn selecting_only_the_second_equally_titled_project_keeps_its_agent_id() -> anyhow::Result<()> {
    rerun_projects("1", &[("t3-notes-2", "p2")])
}

#[test]
fn new_projects_cannot_reuse_an_unselected_projects_agent_id() -> anyhow::Result<()> {
    rerun_projects("1,2", &[("t3-notes-2", "p2"), ("t3-notes-3", "p3")])
}
