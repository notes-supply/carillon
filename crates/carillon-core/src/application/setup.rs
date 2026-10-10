//! `carillon setup`: an installation guided at the terminal. It asks which
//! runtime runs the agent, creates the installation for it, has that
//! runtime set up by its own program, then has Discord's program ask for the
//! bot and who it answers. Each provider's program owns its part; setup only
//! asks the questions that choose between them and runs them in order. A
//! rerun keeps what is already set up when asked to.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context as _, Result, anyhow, bail};
use carillon_installation::{
    config::LoadedConfig, maintenance::MaintenanceLock, prompt::Questions,
};
use carillon_protocol::models::AvailableModel;
use serde::Deserialize;

use super::{
    admin::{Runtime, initialize},
    cli::provider_binary,
};

/// What `carillon setup` was told on its command line.
pub(super) struct Options {
    /// The agent's folder with Pi; a T3 project decides its own.
    pub workspace: Option<PathBuf>,
    pub base_port: u16,
    /// The runtime, when not asked.
    pub runtime: Option<Runtime>,
}

/// What a T3 server offers, as `carillon-runtime-t3 discover` prints it.
#[derive(Deserialize)]
struct Offer {
    projects: Vec<Project>,
    models: Vec<AvailableModel>,
}

#[derive(Deserialize)]
struct Project {
    id: String,
    title: String,
    workspace: PathBuf,
}

/// Sets up the installation at `home`, asking `questions`.
///
/// # Errors
///
/// Returns an error when a question goes unanswered, a provider's program
/// fails, or the installation cannot be created; what was finished stays.
pub(super) fn setup(home: &Path, options: &Options, questions: &mut impl Questions) -> Result<()> {
    let existing = home.join("carillon.toml").exists();
    let runtime = if existing {
        registered_runtime(home)?
    } else if let Some(runtime) = options.runtime {
        runtime
    } else {
        let choices = [
            "Pi, signed in to a model provider such as Anthropic or OpenAI".to_owned(),
            "T3 Code, using a T3 Code server already running on this machine".to_owned(),
        ];
        match questions.choose("Which runtime should run the agent?", &choices, 0)? {
            0 => Runtime::Pi,
            _ => Runtime::T3,
        }
    };
    match runtime {
        Runtime::Pi => {
            if !existing {
                let workspace = match &options.workspace {
                    Some(workspace) => workspace.clone(),
                    None => std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .context("HOME names no folder for the agent; give --workspace")?,
                };
                initialize(home, &workspace, options.base_port, Runtime::Pi)?;
            }
            sign_in_to_pi(home, existing, questions)?;
        }
        Runtime::T3 => {
            if options.workspace.is_some() {
                bail!(
                    "--workspace is for Pi; with T3, the agent works in the chosen project's folder"
                );
            }
            set_up_t3(home, options.base_port, existing, questions)?;
        }
    }
    let discord = Command::new(provider_binary("discord")?)
        .arg("setup")
        .env("CARILLON_HOME", home)
        .status()
        .context("run Discord setup")?;
    if !discord.success() {
        bail!("Discord setup did not finish; run setup again to finish it");
    }
    Ok(())
}

/// The runtime an existing installation chose.
fn registered_runtime(home: &Path) -> Result<Runtime> {
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    [Runtime::Pi, Runtime::T3]
        .into_iter()
        .find(|runtime| runtime.id() == config.value.agent.runtime_provider)
        .with_context(|| {
            format!(
                "the agent runs on {}, which setup does not set up",
                config.value.agent.runtime_provider
            )
        })
}

/// Opens Pi until it is signed in and has a default model, as the Pi
/// runtime's own startup check says.
fn sign_in_to_pi(home: &Path, existing: bool, questions: &mut impl Questions) -> Result<()> {
    let pi = provider_binary("runtime-pi")?;
    let state = home.join("pi");
    let check = || -> Result<Output> {
        Command::new(&pi)
            .arg("--config")
            .arg(home.join("carillon.toml"))
            .args(["--provider", "runtime-pi", "--state-dir"])
            .arg(&state)
            .arg("--check")
            .env("CARILLON_HOME", home)
            .env("PI_CODING_AGENT_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .context("check Pi")
    };
    if existing && check()?.status.success() {
        questions.say("Pi is signed in and has a default model.")?;
        return Ok(());
    }
    loop {
        questions.say(
            "Pi opens next. Sign in with /login, then choose the agent's model with /model and \
             press Ctrl+S on it to save it. Leave Pi with Ctrl+D.",
        )?;
        questions.answer("Press Enter to open Pi.", Some(""))?;
        // Pi's own interface owns the terminal until it ends, however it ends.
        Command::new(&pi)
            .args(["pi", "--no-session", "--no-tools", "--no-approve"])
            .env("CARILLON_HOME", home)
            .env("PI_CODING_AGENT_DIR", &state)
            .status()
            .context("open Pi")?;
        let checked = check()?;
        if checked.status.success() {
            questions.say("Pi is signed in and has a default model.")?;
            return Ok(());
        }
        questions.say(String::from_utf8_lossy(&checked.stderr).trim())?;
        if !questions.confirm("Open Pi again?", true)? {
            bail!("Pi is not ready, so setup stopped here; run it again to finish");
        }
    }
}

/// Asks for a T3 server, then one of its projects and models, creates the
/// installation in that project's folder, and has the T3 runtime write its
/// settings.
fn set_up_t3(
    home: &Path,
    base_port: u16,
    existing: bool,
    questions: &mut impl Questions,
) -> Result<()> {
    if existing
        && home.join("providers/runtime-t3.toml").exists()
        && questions.confirm("Keep the T3 settings?", true)?
    {
        return Ok(());
    }
    let t3 = provider_binary("runtime-t3")?;
    let (url, token, offer) = loop {
        let url = questions.answer("T3 Code's address:", Some("http://127.0.0.1:3773"))?;
        let token = questions.secret(
            "T3 token (`t3 auth session issue --label Carillon --token-only` prints one):",
        )?;
        match run(&t3, &["discover", "--url", &url], &token) {
            Ok(offer) => {
                let offer: Offer = serde_json::from_str(&offer)
                    .context("read what carillon-runtime-t3 discovered")?;
                break (url, token, offer);
            }
            Err(failure) => questions.say(&failure)?,
        }
    };
    // The agent works in the project's folder, which must be here; an
    // existing installation keeps its folder.
    let folder = if existing {
        Some(
            LoadedConfig::load(&home.join("carillon.toml"))?
                .value
                .agent
                .workspace,
        )
    } else {
        None
    };
    let projects: Vec<&Project> = offer
        .projects
        .iter()
        .filter(|project| project.workspace.is_dir())
        .filter(|project| {
            folder
                .as_ref()
                .is_none_or(|folder| same(folder, &project.workspace))
        })
        .collect();
    if projects.is_empty() {
        bail!(match folder {
            Some(folder) => format!(
                "T3 has no project in {}, where the agent works; add one in T3, then run setup \
                 again",
                folder.display()
            ),
            None => "T3 has no project in a folder on this machine; add one in T3 for the \
                     agent's folder, then run setup again"
                .to_owned(),
        });
    }
    if offer.models.is_empty() {
        bail!("T3 offers no usable model; set up a provider in T3, then run setup again");
    }
    let labels: Vec<String> = projects
        .iter()
        .map(|project| format!("{} ({})", project.title, project.workspace.display()))
        .collect();
    let project =
        projects[questions.choose("Which T3 project should the agent work in?", &labels, 0)?];
    let labels: Vec<String> = offer
        .models
        .iter()
        .map(|model| format!("{} ({})", model.name, model.provider))
        .collect();
    let model = &offer.models[questions.choose("Which model should the agent use?", &labels, 0)?];
    if !existing {
        initialize(home, &project.workspace, base_port, Runtime::T3)?;
    }
    // The runtime checks its choice with T3 again and writes its two files,
    // under the maintenance lock, which no question waits behind.
    let _maintenance =
        MaintenanceLock::acquire_unless_held(home).context("the T3 settings were not changed")?;
    let configured = run(
        &t3,
        &[
            "configure",
            "--config",
            &home.join("carillon.toml").to_string_lossy(),
            "--url",
            &url,
            "--project",
            &project.id,
            "--model",
            &format!("{}/{}", model.provider, model.model),
        ],
        &token,
    )
    .map_err(|failure| anyhow!(failure))?;
    questions.say(configured.trim())
}

fn same(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Runs `program` with `secret` on its standard input, never as an
/// argument: what it printed, or the one line it gave as its error.
fn run(program: &Path, args: &[&str], secret: &str) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|failure| format!("could not run {}: {failure}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A program that ends without reading still reports why.
        let _ = writeln!(stdin, "{secret}");
    }
    let output = child
        .wait_with_output()
        .map_err(|failure| format!("{} did not finish: {failure}", program.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let error = String::from_utf8_lossy(&output.stderr);
    Err(error
        .trim()
        .strip_prefix("error: ")
        .unwrap_or_else(|| error.trim())
        .to_owned())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Runtime, initialize, registered_runtime, run};

    #[test]
    fn a_secret_goes_on_standard_input_and_a_failure_is_its_one_line() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let program = directory.path().join("provider");
        std::fs::write(
            &program,
            "#!/bin/sh\nread -r token\n[ \"$token\" = s3cret ] || { echo 'error: T3 refused the token' >&2; exit 1; }\necho \"args: $*\"\n",
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        assert_eq!(
            run(&program, &["discover", "--url", "x"], "s3cret"),
            Ok("args: discover --url x\n".to_owned())
        );
        assert_eq!(
            run(&program, &["discover"], "wrong"),
            Err("T3 refused the token".to_owned())
        );
        Ok(())
    }

    #[test]
    fn a_rerun_keeps_the_runtime_the_installation_chose() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        for runtime in [Runtime::Pi, Runtime::T3] {
            let home = directory.path().join(runtime.id());
            initialize(&home, directory.path(), 44_000, runtime)?;
            assert_eq!(registered_runtime(&home)?, runtime);
        }
        Ok(())
    }
}
