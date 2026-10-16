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
        std::fs::write(
            home.join("providers/runtime-t3.toml"),
            "[agents.t3-notes]\nproject_id = 'p1'\n",
        )?;
        fake_t3(
            directory.path(),
            &serde_json::json!({"projects": [], "models": []}),
        )?;
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

    fn run(&self, workspace: bool) -> anyhow::Result<std::process::Output> {
        let path =
            std::env::join_paths(std::iter::once(self.directory.path().to_path_buf()).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))?;
        let mut child = std::process::Command::new(std::env::current_exe()?);
        child
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("PATH", path)
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
    setup(Path::new(&home), &options, &mut Shown::default())
}

#[test]
fn keeping_t3_while_adding_pi_plans_its_assistant_and_honours_workspace() -> anyhow::Result<()> {
    for ids in [&["t3-notes"][..], &["assistant", "t3-notes"][..]] {
        for workspace in [false, true] {
            let fixture = Fixture::new(ids)?;
            let settings = fixture.home.join("providers/runtime-t3.toml");
            let before = std::fs::read(&settings)?;
            let output = fixture.run(workspace)?;
            assert!(output.status.success(), "{output:?}");
            let config = LoadedConfig::load(&fixture.home.join("carillon.toml"))?;
            let assistant = config.value.agent("assistant")?;
            assert_eq!(assistant.runtime, "runtime-pi");
            let expected = if workspace {
                fixture.workspace.clone()
            } else if ids.contains(&"assistant") {
                fixture.directory.path().to_path_buf()
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
    }
    Ok(())
}

#[test]
fn keeping_t3_refuses_success_if_moving_its_assistant_leaves_it_without_agents()
-> anyhow::Result<()> {
    let fixture = Fixture::new(&["assistant"])?;
    let output = fixture.run(true)?;
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.contains("runtime-t3 has no agent"), "{output}");
    assert!(!fixture.home.join("pi-calls").exists());
    assert!(!fixture.home.join("discord-calls").exists());
    Ok(())
}
