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
        let settings = ids
            .iter()
            .map(|id| {
                let instance = if *id == "assistant" { "carillon_claude" } else { "codex" };
                format!("[agents.{id}]\nproject_id = 'p1'\n[agents.{id}.model]\ninstance_id = '{instance}'\nmodel = 'gpt'\n")
            })
            .collect::<String>();
        let header = "url = 'http://127.0.0.1:3773'\ntoken_file = 'credentials/t3.token'\nprofiles = ['carillon_claude']\n";
        std::fs::write(
            home.join("providers/runtime-t3.toml"),
            format!("{header}{settings}"),
        )?;
        std::fs::write(home.join("credentials/t3.token"), "token")?;
        let t3 = fake_t3(
            directory.path(),
            &serde_json::json!({
                "projects": [{"id": "p1", "title": "Notes", "workspace": directory.path()}],
                "models": [model("codex", "gpt", "GPT")],
            }),
        )?;
        let program = std::fs::read_to_string(&t3)?.replace(
            "configure) echo",
            "configure) while [ \"$1\" != --agent ]; do shift; done; shift; agent=$1; \
             printf 'url = \"http://127.0.0.1:3773\"\\ntoken_file = \"credentials/t3.token\"\\nprofiles = [\"carillon_claude\"]\\n[agents.%s]\\nproject_id = \"p1\"\\n[agents.%s.model]\\ninstance_id = \"codex\"\\nmodel = \"gpt\"\\n' \"$agent\" \"$agent\" \
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

    fn run(&self, workspace: bool, move_assistant: bool) -> anyhow::Result<std::process::Output> {
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
        if move_assistant {
            child.env("CARILLON_TEST_SETUP_MOVE_ASSISTANT", "pi");
        }
        child.output().context("run the setup rerun")
    }
}

#[test]
#[ignore = "run by setup rerun tests with isolated provider programs"]
fn setup_added_pi_child() -> anyhow::Result<()> {
    let home = std::env::var_os("CARILLON_TEST_SETUP_HOME").context("setup home")?;
    let options = Options {
        runtimes: vec![Runtime::Pi],
        workspace: std::env::var_os("CARILLON_TEST_SETUP_WORKSPACE").map(Into::into),
        ..options()
    };
    let mut questions = if std::env::var_os("CARILLON_TEST_SETUP_MOVE_ASSISTANT").is_some() {
        Shown::answering(&["0", "", "token", "0", "0"])
    } else {
        Shown::default()
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
        let output = fixture.run(workspace, false)?;
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
        let output = fixture.run(workspace, true)?;
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
