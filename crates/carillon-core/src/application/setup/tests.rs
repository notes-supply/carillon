use std::{os::unix::fs::PermissionsExt as _, path::Path};

use carillon_installation::config::{LoadedConfig, ProviderRole};

use super::{
    Assistant, Harness, Harnesses, Options, Runtime, initialize, keep_t3, registered_runtimes, run,
    set_up_t3, setup,
};
use crate::application::admin::{ASSISTANT, register_runtime, set_provider};

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
        let config = initialize(&home, directory.path(), 44_000, runtime)?;
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
/// ends the run. Confirmations take their default, a choice is a 0-based
/// index, a multiple choice 0-based indexes joined by commas, and what was
/// shown and asked is kept, a choice's options before its question.
#[derive(Default)]
struct Shown {
    shown: Vec<String>,
    answers: std::collections::VecDeque<&'static str>,
}

impl Shown {
    fn answering(answers: &[&'static str]) -> Self {
        Self {
            shown: Vec::new(),
            answers: answers.iter().copied().collect(),
        }
    }

    fn next(&mut self, question: &str) -> anyhow::Result<&'static str> {
        self.shown.push(question.to_owned());
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("stopped at {question}"))
    }
}

impl carillon_installation::prompt::Questions for Shown {
    fn say(&mut self, text: &str) -> anyhow::Result<()> {
        self.shown.push(text.to_owned());
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
        self.shown.push(question.to_owned());
        Ok(default)
    }

    fn choose(&mut self, question: &str, options: &[String], _: usize) -> anyhow::Result<usize> {
        self.shown.extend(options.iter().cloned());
        Ok(self.next(question)?.parse()?)
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        _: &[usize],
    ) -> anyhow::Result<Vec<usize>> {
        self.shown.extend(options.iter().cloned());
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
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    std::fs::create_dir_all(home.join("providers"))?;
    std::fs::write(home.join("providers/runtime-t3.toml"), "saved")?;
    let program = directory.path().join("carillon-runtime-t3");
    let expected = format!("check --config {}", home.join("carillon.toml").display());
    for (check, shown, kept) in [
        ("exit 0", vec!["Keep the T3 settings and agents?"], true),
        (
            "echo 'error: credentials/t3.token: the T3 token file is empty' >&2; exit 1",
            vec![
                "The saved T3 settings cannot be kept: credentials/t3.token: the T3 token file \
                 is empty",
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
        assert_eq!(questions.shown, shown);
    }
    Ok(())
}

#[test]
fn a_rerun_keeps_the_runtimes_the_installation_registers() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    for runtime in [Runtime::Pi, Runtime::T3] {
        let home = directory.path().join(runtime.id());
        let config = initialize(&home, directory.path(), 44_000, runtime)?;
        assert_eq!(registered_runtimes(&home)?, [runtime]);
        register_runtime(&config, Runtime::T3)?;
        register_runtime(&config, Runtime::Pi)?;
        assert_eq!(registered_runtimes(&home)?, [Runtime::Pi, Runtime::T3]);
    }
    Ok(())
}

/// A fake `carillon-runtime-t3` that records its arguments and standard
/// input, discovers `offer`, and makes profiles in `profiles/`.
fn fake_t3(directory: &Path, offer: &serde_json::Value) -> anyhow::Result<std::path::PathBuf> {
    let program = directory.join("carillon-runtime-t3");
    let calls = directory.join("calls");
    let profiles = directory.join("profiles");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n{{ echo \"$*\"; cat | sed 's/^/  stdin: /'; }} >>'{calls}'\ncase \"$1\" in\n  \
             discover) printf '%s' '{offer}' ;;\n  \
             profile) printf '{{\"instanceId\":\"carillon_claude\",\"home\":\"{profiles}/claude\"}}' ;;\n  \
             configure) echo \"configured $3 $4\" ;;\n  *) exit 2 ;;\nesac\n",
            calls = calls.display(),
            profiles = profiles.display(),
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    Ok(program)
}

fn model(provider: &str, slug: &str, name: &str) -> serde_json::Value {
    serde_json::json!({"provider": provider, "model": slug, "name": name, "options": []})
}

/// A new installation on T3 alone: Carillon's assistant on its own Claude,
/// signed in with a key that goes to T3 alone, and a T3 Code agent for a
/// project, each with a default model among its own instances.
#[test]
fn t3_gets_the_assistant_with_its_profile_and_a_t3_code_agent_per_project() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let assistant = directory.path().join("carillon-workspace");
    let notes = directory.path().join("notes");
    std::fs::create_dir_all(&assistant)?;
    std::fs::create_dir_all(&notes)?;
    let offer = serde_json::json!({
        "projects": [
            {"id": "p-assistant", "title": "Carillon", "workspace": assistant},
            {"id": "p-notes", "title": "Notes App", "workspace": notes},
        ],
        "models": [
            model("claudeAgent", "sonnet", "Sonnet"),
            model("carillon_claude", "sonnet", "Sonnet"),
            model("codex", "gpt", "GPT"),
        ],
    });
    let program = fake_t3(directory.path(), &offer)?;
    // The default address, a token, the assistant's project, Notes for T3
    // Code, a key for Claude, then the assistant's and Notes' first models.
    let mut questions = Shown::answering(&["", "token", "0", "1", "1", "sk-test", "0", "1"]);
    set_up_t3(
        &home,
        44_000,
        &Assistant::T3(Harnesses(vec![Harness::Claude])),
        &program,
        &mut questions,
    )?;
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
            (ASSISTANT, "runtime-t3", assistant.as_path(), true),
            ("t3-notes-app", "runtime-t3", notes.as_path(), false),
        ]
    );
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    let config_arg = home.join("carillon.toml");
    let config_arg = config_arg.display();
    for expected in [
        format!(
            "profile --config {config_arg} --url http://127.0.0.1:3773 --harness claude \
             --with-key\n  stdin: token\n  stdin: sk-test\n"
        ),
        format!(
            "configure --config {config_arg} --url http://127.0.0.1:3773 --agent assistant \
             --project p-assistant --model carillon_claude/sonnet\n"
        ),
        // Plain T3 Code is offered only the instances that are not the profile.
        format!(
            "configure --config {config_arg} --url http://127.0.0.1:3773 --agent t3-notes-app \
             --project p-notes --model codex/gpt\n"
        ),
    ] {
        assert!(calls.contains(&expected), "{expected}\nnot in\n{calls}");
    }
    // The key is the harness's, given to its program on standard input alone.
    assert!(
        !questions
            .shown
            .iter()
            .any(|shown| shown.contains("sk-test"))
    );
    Ok(())
}

/// Models of one T3 instance that share a name are told apart by their
/// slugs, and the one chosen is the one configured.
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
    set_up_t3(&home, 44_000, &Assistant::None, &program, &mut questions)?;
    let options = [
        "Sonnet (claudeAgent/sonnet-a)",
        "Sonnet (claudeAgent/sonnet-b)",
        "Sonnet (other)",
        "Which model should T3 Code · Agent start new conversations with?",
    ];
    assert!(
        questions.shown.windows(4).any(|shown| shown == options),
        "{:?}",
        questions.shown
    );
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    assert!(calls.contains("--model claudeAgent/sonnet-b"), "{calls}");
    // With no assistant, the installation's only agent is T3 Code's.
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

/// With nothing chosen for T3 to run, setup says so instead of registering it.
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
        44_000,
        &Assistant::None,
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
    set_up_t3(&home, 44_000, &Assistant::Pi, &program, &mut questions)?;
    assert!(
        questions
            .shown
            .contains(&"T3 Code is not used, so it was not set up.".to_owned())
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
    set_up_t3(&home, 44_000, &Assistant::None, &program, &mut questions)?;
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
