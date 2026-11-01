use carillon_installation::config::AgentConfig;

use super::*;
use crate::application::setup::choose_pi_models;

/// A fake `carillon-runtime-pi` that offers two models, keeps each agent's
/// chosen one in a file of its own, and records every call with Pi's state
/// folder.
fn fake_pi(directory: &Path) -> anyhow::Result<PathBuf> {
    let program = directory.join("carillon-runtime-pi");
    std::fs::write(
        &program,
        format!(
            r#"#!/bin/sh
echo "$PI_CODING_AGENT_DIR $*" >>'{dir}/calls'
agent=; model=; mode=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent) shift; agent=$1 ;;
    --model) shift; model=$1 ;;
    --models|--configure) mode=$1 ;;
  esac
  shift
done
value=null
[ -f '{dir}/value-'"$agent" ] && value="\"$(cat '{dir}/value-'"$agent")\""
case "$mode" in
  --models) printf '{{"models": [%s, %s], "value": %s}}\n' \
    '{{"provider": "openai-codex", "model": "gpt-6.1-sol", "name": "GPT-6.1 Sol"}}' \
    '{{"provider": "openai-codex", "model": "gpt-6.1-mini", "name": "GPT-6.1 Mini"}}' "$value" ;;
  --configure) printf '%s' "$model" >'{dir}/value-'"$agent"; echo "$agent uses $model" ;;
  *) exit 2 ;;
esac
"#,
            dir = directory.display()
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    Ok(program)
}

fn agent(id: &str, name: &str, runtime: Runtime) -> AgentConfig {
    AgentConfig {
        id: id.to_owned(),
        name: name.to_owned(),
        runtime: runtime.id().to_owned(),
        workspace: PathBuf::from("/"),
        resources: None,
    }
}

/// Setup asks the model of each Pi agent that uses none and has Pi keep it
/// as the agent's own. A rerun asks nothing, and an agent on another
/// runtime is never asked here.
#[test]
fn each_pi_agent_is_asked_its_model_until_it_has_one() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let pi = fake_pi(directory.path())?;
    let agents = [
        agent("assistant", "Assistant", Runtime::Pi),
        agent("helper", "Helper", Runtime::Pi),
        agent("t3-notes", "T3 Code · Notes", Runtime::T3),
    ];
    // Mini for the assistant, then Sol for the helper.
    let mut questions = Shown::answering(&["1", "0"]);
    choose_pi_models(&home, &pi, &agents, &mut questions)?;
    let offered = ["GPT-6.1 Sol (openai-codex)", "GPT-6.1 Mini (openai-codex)"];
    assert_eq!(
        questions.shown,
        [
            offered[0],
            offered[1],
            "Which model should Assistant use?",
            "assistant uses openai-codex/gpt-6.1-mini",
            offered[0],
            offered[1],
            "Which model should Helper use?",
            "helper uses openai-codex/gpt-6.1-sol",
        ]
    );
    let state = home.join("pi");
    let pi_flags = format!(
        "{state} --config {config} --provider runtime-pi --state-dir {state}",
        state = state.display(),
        config = home.join("carillon.toml").display(),
    );
    let calls = directory.path().join("calls");
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        format!(
            "{pi_flags} --models --agent assistant\n\
             {pi_flags} --configure --agent assistant --model openai-codex/gpt-6.1-mini\n\
             {pi_flags} --models --agent helper\n\
             {pi_flags} --configure --agent helper --model openai-codex/gpt-6.1-sol\n"
        )
    );
    std::fs::remove_file(&calls)?;
    choose_pi_models(&home, &pi, &agents, &mut Unasked)?;
    assert_eq!(
        std::fs::read_to_string(&calls)?,
        format!("{pi_flags} --models --agent assistant\n{pi_flags} --models --agent helper\n")
    );
    Ok(())
}
