use anyhow::Context as _;

use super::*;

const CHILD: &str = "application::setup::tests::added_pi::setup_added_pi_child";

struct Fixture {
    directory: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    /// An installation of T3 Code agent `t3-notes` alone, whose T3 settings
    /// the runtime's check reads.
    fn new() -> anyhow::Result<Self> {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        let workspace = directory.path().join("requested-workspace");
        std::fs::create_dir(&workspace)?;
        let path = initialized(&home, directory.path(), Runtime::T3)?;
        crate::application::admin::set_agents(
            &path,
            vec![carillon_installation::config::AgentConfig {
                id: "t3-notes".to_owned(),
                name: "T3 Code · Notes".to_owned(),
                runtime: Runtime::T3.id().to_owned(),
                workspace: directory.path().to_path_buf(),
                resources: None,
            }],
        )?;
        std::fs::write(
            home.join("providers/runtime-t3.toml"),
            "url = 'http://127.0.0.1:3773'\ntoken_file = 'credentials/t3.token'\n\
             [agents.t3-notes]\nproject_id = 'p1'\n",
        )?;
        std::fs::write(home.join("credentials/t3.token"), "token")?;
        fake_t3(
            directory.path(),
            &serde_json::json!({
                "projects": [{"id": "p1", "title": "Notes", "workspace": directory.path()}],
                "models": [model("codex", "gpt", "GPT")],
            }),
        )?;
        for (name, script) in [
            (
                "runtime-pi",
                "echo \"$*\" >>\"$CARILLON_HOME/pi-calls\"\n\
                 case \"$*\" in *--models*) echo '{\"models\": [], \"value\": \"openai-codex/gpt-6.1-sol\"}' ;; esac",
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

    fn run(&self, workspace: bool) -> anyhow::Result<std::process::Output> {
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
    let mut questions = Shown::default();
    setup(Path::new(&home), &options, &mut questions)?;
    println!("{}", questions.transcript.join("\n"));
    Ok(())
}

#[test]
fn keeping_t3_while_adding_pi_plans_its_assistant_and_honours_workspace() -> anyhow::Result<()> {
    for workspace in [false, true] {
        let fixture = Fixture::new()?;
        let settings = fixture.home.join("providers/runtime-t3.toml");
        let before = std::fs::read(&settings)?;
        let output = fixture.run(workspace)?;
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
        let pi = std::fs::read_to_string(fixture.home.join("pi-calls"))?;
        assert!(pi.contains("--check"), "{pi}");
        assert!(pi.contains("--models --agent assistant"), "{pi}");
        assert!(!pi.contains("--configure"), "{pi}");
        assert_eq!(
            std::fs::read_to_string(fixture.home.join("discord-calls"))?,
            "setup\n"
        );
    }
    Ok(())
}
