use std::fmt::Write as _;

use super::*;

const CHILD: &str = "application::setup::tests::added_pi::setup_added_pi_child";

struct Fixture {
    directory: tempfile::TempDir,
    home: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl Fixture {
    fn new(ids: &[&str]) -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        let workspace = directory.path().join("requested-workspace");
        std::fs::create_dir(&workspace)?;
        let path = initialize(&home, directory.path(), 44_000, Runtime::T3)?;
        let mut config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let assistant = config.agents.remove(0);
        crate::application::admin::set_agents(
            &path,
            ids.iter()
                .map(|id| {
                    let mut agent = assistant.clone();
                    (*id).clone_into(&mut agent.id);
                    if *id != "assistant" {
                        agent.resources = None;
                    }
                    agent
                })
                .collect(),
        )?;
        let mut settings = String::from(
            "url = 'http://127.0.0.1:3773'\ntoken_file = 'credentials/t3.token'\nprofiles = ['carillon_claude']\n",
        );
        for id in ids {
            let instance = if *id == "assistant" {
                "carillon_claude"
            } else {
                "codex"
            };
            writeln!(
                settings,
                "[agents.{id}]\nproject_id = 'p1'\n[agents.{id}.model]\ninstance_id = '{instance}'\nmodel = 'gpt'"
            )?;
        }
        std::fs::write(home.join("providers/runtime-t3.toml"), settings)?;
        std::fs::write(home.join("credentials/t3.token"), "token")?;
        let t3 = fake_t3(
            directory.path(),
            &serde_json::json!({
                "projects": [{"id": "p1", "title": "Notes", "workspace": directory.path()}],
                "models": [model("codex", "gpt", "GPT"), model("carillon_claude", "gpt", "Profile GPT")],
            }),
        )?;
        let program = std::fs::read_to_string(&t3)?.replace(
            "configure) echo",
            "configure) while [ \"$#\" -gt 0 ]; do case \"$1\" in \
             --agent) shift; agent=$1 ;; --model) shift; model=$1 ;; esac; shift; done; \
             printf 'url = \"http://127.0.0.1:3773\"\\ntoken_file = \"credentials/t3.token\"\\nprofiles = [\"carillon_claude\"]\\n[agents.%s]\\nproject_id = \"p1\"\\n[agents.%s.model]\\ninstance_id = \"%s\"\\nmodel = \"%s\"\\n' \"$agent\" \"$agent\" \"${model%/*}\" \"${model#*/}\" \
             >\"$CARILLON_HOME/providers/runtime-t3.toml\"; echo",
        );
        std::fs::write(t3, program)?;
        for (name, script) in [
            (
                "runtime-pi",
                "echo \"$*\" >>\"$CARILLON_HOME/pi-calls\"\nexit 0",
            ),
            (
                "discord",
                "echo \"$*\" >>\"$CARILLON_HOME/discord-calls\"\nexit 0",
            ),
        ] {
            let program = directory.path().join(format!("carillon-{name}"));
            std::fs::write(&program, format!("#!/bin/sh\n{script}\n"))?;
            std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            directory,
            home,
            workspace,
        })
    }

    fn run(
        &self,
        workspace: bool,
        move_assistant: Option<Runtime>,
    ) -> anyhow::Result<std::process::Output> {
        let path =
            std::env::join_paths(std::iter::once(self.directory.path().to_path_buf()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))?;
        let mut child = std::process::Command::new(std::env::current_exe()?);
        child
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("PATH", path)
            .env("CARILLON_HOME", &self.home)
            .env("CARILLON_TEST_SETUP_HOME", &self.home);
        if workspace {
            child.env("CARILLON_TEST_SETUP_WORKSPACE", &self.workspace);
        }
        if let Some(runtime) = move_assistant {
            child.env("CARILLON_TEST_SETUP_MOVE_ASSISTANT", runtime.id());
        }
        child.output().context("run the setup rerun")
    }
}

#[test]
#[ignore = "run by setup rerun tests with isolated provider programs"]
fn setup_added_pi_child() -> anyhow::Result<()> {
    let home = std::env::var_os("CARILLON_TEST_SETUP_HOME").context("setup home")?;
    let moving = std::env::var("CARILLON_TEST_SETUP_MOVE_ASSISTANT").unwrap_or_default();
    let options = Options {
        runtimes: if moving == "runtime-t3" {
            Vec::new()
        } else {
            vec![Runtime::Pi]
        },
        workspace: std::env::var_os("CARILLON_TEST_SETUP_WORKSPACE").map(Into::into),
        ..options()
    };
    let mut questions = match moving.as_str() {
        "runtime-pi" => Shown::answering(&["0", "", "token", "0", "0"]),
        "runtime-t3" => Shown::answering(&["1", "0", "", "token", "0", "", "1", "key", "1"]),
        _ => Shown::default(),
    };
    setup(Path::new(&home), &options, &mut questions)?;
    println!("{}", questions.shown.join("\n"));
    Ok(())
}

#[test]
fn keeping_t3_while_adding_pi_plans_its_assistant_and_honours_workspace() -> anyhow::Result<()> {
    for workspace in [false, true] {
        let fixture = Fixture::new(&["t3-notes"])?;
        let settings = fixture.home.join("providers/runtime-t3.toml");
        let before = std::fs::read(&settings)?;
        let output = fixture.run(workspace, None)?;
        assert!(output.status.success(), "{output:?}");
        let config = LoadedConfig::load(&fixture.home.join("carillon.toml"))?;
        let assistant = config.value.agent("assistant")?;
        assert_eq!(assistant.runtime, "runtime-pi");
        let expected = if workspace {
            fixture.workspace.clone()
        } else {
            std::env::var_os("HOME").context("HOME")?.into()
        };
        assert_eq!(assistant.workspace, expected);
        assert_eq!(config.value.agent("t3-notes")?.runtime, "runtime-t3");
        assert_eq!(config.value.agents.len(), 2);
        assert_eq!(std::fs::read(&settings)?, before);
        assert!(std::fs::read_to_string(fixture.home.join("pi-calls"))?.contains("--check"));
        assert_eq!(
            std::fs::read_to_string(fixture.home.join("discord-calls"))?,
            "setup\n"
        );
    }
    Ok(())
}

#[test]
fn adding_pi_to_a_t3_assistant_reconfigures_t3_before_moving_it() -> anyhow::Result<()> {
    for workspace in [false, true] {
        let fixture = Fixture::new(&["assistant", "t3-notes"])?;
        let output = fixture.run(workspace, Some(Runtime::Pi))?;
        assert!(output.status.success(), "{output:?}");
        let shown = String::from_utf8_lossy(&output.stdout);
        assert!(
            !shown.contains("Keep the T3 settings and agents?"),
            "{shown}"
        );
        assert!(
            shown.contains("What should run Carillon's assistant?"),
            "{shown}"
        );
        let config = LoadedConfig::load(&fixture.home.join("carillon.toml"))?;
        let assistant = config.value.agent("assistant")?;
        assert_eq!(assistant.runtime, "runtime-pi");
        assert_eq!(
            assistant.workspace,
            if workspace {
                fixture.workspace.clone()
            } else {
                fixture.directory.path().to_path_buf()
            }
        );
        let saved = super::super::saved_projects(&fixture.home)?;
        assert_eq!(
            saved.keys().map(String::as_str).collect::<Vec<_>>(),
            ["t3-notes"]
        );
        for id in saved.keys() {
            assert_eq!(config.value.agent(id)?.runtime, "runtime-t3");
        }
        run(
            &fixture.directory.path().join("carillon-runtime-t3"),
            &[
                "check",
                "--config",
                &fixture.home.join("carillon.toml").to_string_lossy(),
            ],
            "",
        )?;
        let calls = std::fs::read_to_string(fixture.directory.path().join("calls"))?;
        assert!(calls.contains("configure --config"), "{calls}");
        assert!(
            calls.contains("--agent t3-notes --project p1 --model codex/gpt"),
            "{calls}"
        );
        assert!(calls.contains("check --config"), "{calls}");
        assert_eq!(
            std::fs::read_to_string(fixture.home.join("discord-calls"))?,
            "setup\n"
        );
    }
    Ok(())
}

#[test]
fn moving_the_assistant_to_t3_leaves_pi_registered_and_idle() -> anyhow::Result<()> {
    let fixture = Fixture::new(&["assistant"])?;
    let path = fixture.home.join("carillon.toml");
    register_runtime(&path, Runtime::Pi)?;
    let mut config: Config = toml::from_str(&std::fs::read_to_string(&path)?)?;
    config.agents[0].runtime = Runtime::Pi.id().to_owned();
    crate::application::admin::set_agents(&path, config.agents)?;
    std::fs::remove_file(fixture.home.join("providers/runtime-t3.toml"))?;

    let output = fixture.run(false, Some(Runtime::T3))?;
    assert!(output.status.success(), "{output:?}");
    let config = LoadedConfig::load(&path)?;
    assert_eq!(config.value.agent("assistant")?.runtime, "runtime-t3");
    assert_eq!(config.value.agents.len(), 1);
    assert!(
        config
            .provider("runtime-pi", ProviderRole::Runtime)?
            .enabled
    );
    assert_eq!(
        registered_runtimes(&fixture.home)?,
        [Runtime::Pi, Runtime::T3]
    );
    assert!(!fixture.home.join("pi-calls").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.home.join("discord-calls"))?,
        "setup\n"
    );
    Ok(())
}
