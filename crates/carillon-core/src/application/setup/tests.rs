use std::{
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use carillon_installation::config::{Config, LoadedConfig, ProviderRole};

use super::{Options, Runtime, initialize, keep_t3, registered_runtimes, run, set_up_t3, setup};
use crate::application::admin::{ASSISTANT, assistant, backup, register_runtime, set_provider};

/// A new installation on `runtime`: on Pi with Carillon's assistant working
/// in `workspace`, on T3 with no agent yet.
fn initialized(home: &Path, workspace: &Path, runtime: Runtime) -> anyhow::Result<PathBuf> {
    let agents = if runtime == Runtime::Pi {
        vec![assistant(workspace)]
    } else {
        Vec::new()
    };
    initialize(home, 44_000, runtime, agents)
}

#[test]
fn a_secret_goes_on_standard_input_and_a_failure_is_its_one_line() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = directory.path().join("provider");
    std::fs::write(
        &program,
        "#!/bin/sh\nread -r token\n[ \"$token\" = s3cret ] || { echo 'error: T3 refused the token' >&2; exit 1; }\necho \"args: $*\"\n",
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    let ran = |args: &[&str], secret: &str| {
        run(&program, args, secret).map_err(|failure| format!("{failure:#}"))
    };
    assert_eq!(
        ran(&["discover", "--url", "x"], "s3cret"),
        Ok("args: discover --url x\n".to_owned())
    );
    assert_eq!(
        ran(&["discover"], "wrong"),
        Err("T3 refused the token".to_owned())
    );
    Ok(())
}

/// Questions nobody should be asked: asking any is an error that names it.
struct Unasked;

impl carillon_installation::prompt::Questions for Unasked {
    fn say(&mut self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn answer(&mut self, question: &str, _: Option<&str>) -> anyhow::Result<String> {
        anyhow::bail!("asked: {question}")
    }

    fn secret(&mut self, question: &str) -> anyhow::Result<String> {
        anyhow::bail!("asked: {question}")
    }

    fn confirm(&mut self, question: &str, _: bool) -> anyhow::Result<bool> {
        anyhow::bail!("asked: {question}")
    }

    fn choose(&mut self, question: &str, _: &[String], _: usize) -> anyhow::Result<usize> {
        anyhow::bail!("asked: {question}")
    }

    fn choose_many(
        &mut self,
        question: &str,
        _: &[String],
        _: &[usize],
    ) -> anyhow::Result<Vec<usize>> {
        anyhow::bail!("asked: {question}")
    }
}

/// A rerun with a registered runtime disabled stops before opening Pi or
/// asking for T3, and says how to enable it; it does not enable it.
#[test]
fn a_disabled_runtime_stops_setup_before_it_opens_anything() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    for runtime in [Runtime::Pi, Runtime::T3] {
        let home = directory.path().join(runtime.id());
        let config = initialized(&home, directory.path(), runtime)?;
        set_provider(&config, runtime.id(), false)?;
        let options = Options {
            workspace: None,
            base_port: 44_000,
            runtimes: Vec::new(),
        };
        let refusal = setup(&home, &options, &mut Unasked)
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert_eq!(
            refusal,
            format!(
                "{id} is disabled in carillon.toml; enable it with `carillon provider {id} \
                 enable`, then run setup again",
                id = runtime.id()
            )
        );
        // Still disabled: enabling it is the operator's decision.
        let registry = LoadedConfig::load(&config)?;
        assert!(
            !registry
                .provider(runtime.id(), ProviderRole::Runtime)?
                .enabled
        );
    }
    Ok(())
}

/// Questions answered in order from `answers`; the first left without one
/// ends the run. Confirmations take `confirmations`, then their default, a
/// choice is a 0-based index, a multiple choice 0-based indexes joined by
/// commas, and what was shown and asked is kept, a choice's options before
/// its question.
#[derive(Default)]
struct Shown {
    transcript: Vec<String>,
    answers: std::collections::VecDeque<&'static str>,
    /// Answers to confirmations, in order, before their defaults.
    confirmations: std::collections::VecDeque<bool>,
}

impl Shown {
    fn answering(answers: &[&'static str]) -> Self {
        Self {
            answers: answers.iter().copied().collect(),
            ..Self::default()
        }
    }

    fn next(&mut self, question: &str) -> anyhow::Result<&'static str> {
        self.transcript.push(question.to_owned());
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("stopped at {question}"))
    }
}

impl carillon_installation::prompt::Questions for Shown {
    fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.transcript.push(text.to_owned());
        Ok(())
    }

    fn answer(&mut self, question: &str, default: Option<&str>) -> anyhow::Result<String> {
        let answer = self.next(question)?;
        Ok(if answer.is_empty() {
            default.unwrap_or_default().to_owned()
        } else {
            answer.to_owned()
        })
    }

    fn secret(&mut self, question: &str) -> anyhow::Result<String> {
        Ok(self.next(question)?.to_owned())
    }

    fn confirm(&mut self, question: &str, default: bool) -> anyhow::Result<bool> {
        self.transcript.push(question.to_owned());
        Ok(self.confirmations.pop_front().unwrap_or(default))
    }

    fn choose(&mut self, question: &str, options: &[String], _: usize) -> anyhow::Result<usize> {
        self.transcript.extend(options.iter().cloned());
        Ok(self.next(question)?.parse()?)
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        _: &[usize],
    ) -> anyhow::Result<Vec<usize>> {
        self.transcript.extend(options.iter().cloned());
        self.next(question)?
            .split(',')
            .filter(|index| !index.is_empty())
            .map(|index| Ok(index.parse()?))
            .collect()
    }
}

/// Saved T3 settings are offered for keeping only when the runtime's own
/// check reads them; otherwise setup says why and sets T3 up again, so an
/// unusable token can be replaced.
#[test]
fn saved_t3_settings_are_offered_only_when_the_runtime_reads_them() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialized(&home, directory.path(), Runtime::T3)?;
    std::fs::create_dir_all(home.join("providers"))?;
    std::fs::write(home.join("providers/runtime-t3.toml"), "saved")?;
    let program = directory.path().join("carillon-runtime-t3");
    let expected = format!("check --config {}", home.join("carillon.toml").display());
    for (check, shown, kept) in [
        ("exit 0", vec!["Keep the T3 settings and agents?"], true),
        (
            "echo 'error: credentials/t3.token: the T3 token file is empty' >&2; exit 1",
            vec![
                "The saved T3 settings cannot be kept, so setup replaces them: \
                 credentials/t3.token: the T3 token file is empty",
            ],
            false,
        ),
    ] {
        std::fs::write(
            &program,
            format!("#!/bin/sh\n[ \"$*\" = '{expected}' ] || exit 2\n{check}\n"),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let mut questions = Shown::default();
        assert_eq!(keep_t3(&home, &program, &mut questions)?, kept);
        assert_eq!(questions.transcript, shown);
        assert_eq!(home.join("providers/runtime-t3.toml").exists(), kept);
    }
    Ok(())
}

#[test]
fn a_rerun_keeps_the_runtimes_the_installation_registers() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    for runtime in [Runtime::Pi, Runtime::T3] {
        let home = directory.path().join(runtime.id());
        let config = initialized(&home, directory.path(), runtime)?;
        assert_eq!(registered_runtimes(&home)?, [runtime]);
        register_runtime(&config, Runtime::T3)?;
        register_runtime(&config, Runtime::Pi)?;
        assert_eq!(registered_runtimes(&home)?, [Runtime::Pi, Runtime::T3]);
    }
    Ok(())
}

/// A fake `carillon-runtime-t3` that records its arguments and standard
/// input and discovers `offer`, or for an agent its `agentModels` entry.
/// Its `check` and `configure` refuse settings that name an agent's model,
/// as an earlier release wrote them.
fn fake_t3(directory: &Path, offer: &serde_json::Value) -> anyhow::Result<PathBuf> {
    let program = directory.join("carillon-runtime-t3");
    let calls = directory.join("calls");
    if let Some(offers) = offer
        .get("agentModels")
        .and_then(serde_json::Value::as_object)
    {
        for (id, models) in offers {
            std::fs::write(
                directory.join(format!("offer-{id}.json")),
                serde_json::to_vec(
                    &serde_json::json!({"projects": offer["projects"], "models": models}),
                )?,
            )?;
        }
    }
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n{{ echo \"$*\"; cat | sed 's/^/  stdin: /'; }} >>'{calls}'\ncase \"$1\" in\n  \
             discover) agent=; while [ \"$#\" -gt 0 ]; do if [ \"$1\" = --agent ]; then shift; agent=$1; fi; shift; done; if [ -n \"$agent\" ] && [ -f '{directory}/offer-'\"$agent\"'.json' ]; then cat '{directory}/offer-'\"$agent\"'.json'; else printf '%s' '{offer}'; fi ;;\n  \
             check|configure) settings=\"${{3%/carillon.toml}}/providers/runtime-t3.toml\"; \
             if grep -qs '\\.model]' \"$settings\"; then echo \"error: $1 refuses $settings\" >&2; exit 1; fi; \
             [ \"$1\" = check ] || echo \"configured $3 $4\" ;;\n  *) exit 2 ;;\nesac\n",
            directory = directory.display(),
            calls = calls.display(),
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    Ok(program)
}

fn model(provider: &str, slug: &str, name: &str) -> serde_json::Value {
    serde_json::json!({"provider": provider, "model": slug, "name": name, "options": []})
}

/// With Pi and T3, T3 asks only for its address, its token and the projects
/// T3 Code channels work in: Carillon's assistant runs on Pi, and each
/// project's agent gets a default model among those T3 offers it.
#[test]
fn t3_code_gets_an_agent_per_project_beside_the_assistant_on_pi() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let workspace = directory.path().join("carillon-workspace");
    let notes = directory.path().join("notes");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&notes)?;
    let offer = serde_json::json!({
        "projects": [
            {"id": "p-other", "title": "Other", "workspace": workspace},
            {"id": "p-notes", "title": "Notes App", "workspace": notes},
        ],
        "models": [model("claudeAgent", "sonnet", "Sonnet"), model("codex", "gpt", "GPT")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    // The default address, a token, Notes for T3 Code, then its second model.
    let mut questions = Shown::answering(&["", "token", "1", "1"]);
    let options = Options {
        workspace: Some(workspace.clone()),
        ..options()
    };
    set_up_t3(&home, &options, true, &program, &mut questions)?;
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    let agents: Vec<(&str, &str, &Path, bool)> = config
        .value
        .agents
        .iter()
        .map(|agent| {
            (
                agent.id.as_str(),
                agent.runtime.as_str(),
                agent.workspace.as_path(),
                agent.resources.is_some(),
            )
        })
        .collect();
    assert_eq!(
        agents,
        [
            (ASSISTANT, "runtime-pi", workspace.as_path(), true),
            ("t3-notes-app", "runtime-t3", notes.as_path(), false),
        ]
    );
    let stored: Config = toml::from_str(&std::fs::read_to_string(home.join("carillon.toml"))?)?;
    assert_eq!(
        stored.agent(ASSISTANT)?.resources.as_deref(),
        Some(Path::new("agents/default"))
    );
    backup(&config, &directory.path().join("backup.tar.gz"))?;
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    let expected = format!(
        "configure --config {} --url http://127.0.0.1:3773 --agent t3-notes-app \
         --project p-notes --model codex/gpt\n",
        home.join("carillon.toml").display()
    );
    assert!(calls.contains(&expected), "{expected}\nnot in\n{calls}");
    assert!(!calls.contains("--agent assistant"), "{calls}");
    assert_eq!(
        questions.transcript,
        [
            "T3 Code's address:",
            "T3 token (`t3 auth session issue --label Carillon --token-only` prints one):",
            &format!("Other ({})", workspace.display()),
            &format!("Notes App ({})", notes.display()),
            "Which T3 projects should T3 Code channels work in? Each is an agent Discord can \
             route channels to.",
            "Sonnet (claudeAgent)",
            "GPT (codex)",
            "Which model should T3 Code · Notes App start new conversations with?",
            &format!("configured {} --url", home.join("carillon.toml").display()),
        ]
    );
    Ok(())
}

#[test]
fn models_sharing_a_name_are_told_apart_and_the_one_chosen_is_configured() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let offer = serde_json::json!({
        "projects": [{"id": "agent", "title": "Agent", "workspace": directory.path()}],
        "models": [
            model("claudeAgent", "sonnet-a", "Sonnet"),
            model("claudeAgent", "sonnet-b", "Sonnet"),
            model("other", "sonnet", "Sonnet"),
        ],
    });
    let program = fake_t3(directory.path(), &offer)?;
    // The default address, a token, the only project for T3 Code, the second Sonnet.
    let mut questions = Shown::answering(&["", "token", "0", "1"]);
    set_up_t3(&home, &options(), false, &program, &mut questions)?;
    let options = [
        "Sonnet (claudeAgent/sonnet-a)",
        "Sonnet (claudeAgent/sonnet-b)",
        "Sonnet (other)",
        "Which model should T3 Code · Agent start new conversations with?",
    ];
    assert!(
        questions
            .transcript
            .windows(4)
            .any(|shown| shown == options),
        "{:?}",
        questions.transcript
    );
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    assert!(calls.contains("--model claudeAgent/sonnet-b"), "{calls}");
    // Without Pi, the installation's only agent is T3 Code's.
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    let ids: Vec<&str> = config
        .value
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect();
    assert_eq!(ids, ["t3-agent"]);
    Ok(())
}

#[test]
fn t3_with_nothing_to_run_is_refused_or_left_out() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let offer = serde_json::json!({
        "projects": [{"id": "agent", "title": "Agent", "workspace": directory.path()}],
        "models": [model("claudeAgent", "sonnet", "Sonnet")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    let refused = set_up_t3(
        &home,
        &options(),
        false,
        &program,
        &mut Shown::answering(&["", "token", ""]),
    );
    let refusal = refused
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    assert!(refusal.contains("nothing runs on T3"), "{refusal}");
    assert!(!home.exists());
    let mut questions = Shown::answering(&["", "token", ""]);
    set_up_t3(&home, &options(), true, &program, &mut questions)?;
    assert!(
        questions
            .transcript
            .contains(&"T3 Code has no agents to configure.".to_owned())
    );
    Ok(())
}

/// T3 Code agents are named from their projects' titles, as lowercase
/// words; titles that name the same agent are told apart by a number, and
/// one with no word at all is a project.
#[test]
fn t3_code_agents_are_named_from_their_projects_without_clashing() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let offer = serde_json::json!({
        "projects": [
            {"id": "p1", "title": "Notes App (beta)", "workspace": directory.path()},
            {"id": "p2", "title": "notes app, beta", "workspace": directory.path()},
            {"id": "p3", "title": "✨", "workspace": directory.path()},
        ],
        "models": [model("claudeAgent", "sonnet", "Sonnet")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    // The default address, a token, every project for T3 Code, then each
    // agent's only model.
    let mut questions = Shown::answering(&["", "token", "0,1,2", "0", "0", "0"]);
    set_up_t3(&home, &options(), false, &program, &mut questions)?;
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    let agents: Vec<(&str, &str)> = config
        .value
        .agents
        .iter()
        .map(|agent| (agent.id.as_str(), agent.name.as_str()))
        .collect();
    assert_eq!(
        agents,
        [
            ("t3-notes-app-beta", "T3 Code · Notes App (beta)"),
            ("t3-notes-app-beta-2", "T3 Code · notes app, beta"),
            ("t3-project", "T3 Code · ✨"),
        ]
    );
    Ok(())
}

fn options() -> Options {
    Options {
        workspace: None,
        base_port: 44_000,
        runtimes: Vec::new(),
    }
}

#[test]
fn equally_named_agents_get_distinct_model_prompts_and_their_chosen_models() -> anyhow::Result<()> {
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
            {"id": "p3", "title": "Other", "workspace": directory.path()},
        ],
        "models": [model("claudeAgent", "sonnet", "Sonnet"), model("codex", "gpt", "GPT")],
    });
    let program = fake_t3(directory.path(), &offer)?;
    let mut questions = Shown::answering(&["", "token", "0,1,2", "0", "1", "0"]);
    set_up_t3(&home, &options(), false, &program, &mut questions)?;
    for label in [
        "T3 Code · Notes (t3-notes)",
        "T3 Code · Notes (t3-notes-2)",
        "T3 Code · Other",
    ] {
        assert!(
            questions.transcript.contains(&format!(
                "Which model should {label} start new conversations with?"
            )),
            "{:?}",
            questions.transcript
        );
    }
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    for args in [
        "--agent t3-notes --project p1 --model claudeAgent/sonnet",
        "--agent t3-notes-2 --project p2 --model codex/gpt",
        "--agent t3-other --project p3 --model claudeAgent/sonnet",
    ] {
        assert!(calls.contains(args), "{calls}");
    }
    Ok(())
}

mod added_pi;
mod pi_models;
mod projects;
mod reruns;
