//! `carillon setup`: an installation guided at the terminal. It asks which
//! runtimes Carillon uses, has each runtime set up by its own program, Pi
//! for Carillon's assistant and T3 for the projects T3 Code channels work
//! in, then has Discord's program ask for the bot and who answers in which
//! channel. Each provider's program owns its part; setup only asks the
//! questions that choose between them and runs them in order. A rerun keeps
//! what is already set up when asked to.

use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    config::{AgentConfig, Config, LoadedConfig, ProviderRole},
    maintenance::MaintenanceLock,
    prompt::Questions,
};
use carillon_protocol::models::AvailableModel;
use serde::Deserialize;

use super::{
    admin::{self, Runtime, initialize, register_runtime, set_agents},
    cli::provider_binary,
};

/// What `carillon setup` was told on its command line.
pub(super) struct Options {
    /// The assistant's folder with Pi; a T3 project decides its own.
    pub workspace: Option<PathBuf>,
    pub base_port: u16,
    /// The runtimes, when not asked.
    pub runtimes: Vec<Runtime>,
}

/// What a T3 server offers, as `carillon-runtime-t3 discover` prints it.
#[derive(Deserialize)]
struct Offer {
    projects: Vec<Project>,
    models: Vec<AvailableModel>,
}

#[derive(Clone, Deserialize)]
struct Project {
    id: String,
    title: String,
    workspace: PathBuf,
}

/// # Errors
///
/// Returns an error when a question goes unanswered, a provider's program
/// fails, or the installation cannot be created; what was finished stays.
pub(super) fn setup(home: &Path, options: &Options, questions: &mut impl Questions) -> Result<()> {
    let existing = home.join("carillon.toml").exists();
    let runtimes = runtimes(home, options)?;
    let runtimes = if runtimes.is_empty() {
        ask_runtimes(questions)?
    } else {
        runtimes
    };
    // Carillon's assistant runs on Pi, whenever Pi is used.
    let pi = runtimes.contains(&Runtime::Pi);
    let t3 = runtimes.contains(&Runtime::T3);
    if options.workspace.is_some() && !pi {
        bail!("--workspace is for Pi; on T3, an agent works in its project's folder");
    }
    let kept = t3 && existing && keep_t3(home, &provider_binary("runtime-t3")?, questions)?;
    if t3 && !kept {
        set_up_t3(
            home,
            options,
            pi,
            &provider_binary("runtime-t3")?,
            questions,
        )?;
    } else if pi {
        agents(home, options, true, None)?;
    }
    {
        let _maintenance = MaintenanceLock::acquire_unless_held(home)?;
        for &runtime in &runtimes {
            register_runtime(&home.join("carillon.toml"), runtime)?;
        }
    }
    sign_in_to_pi(home, existing, questions)?;
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

fn runtimes(home: &Path, options: &Options) -> Result<Vec<Runtime>> {
    let mut runtimes = if home.join("carillon.toml").exists() {
        registered_runtimes(home)?
    } else {
        Vec::new()
    };
    for &runtime in &options.runtimes {
        if !runtimes.contains(&runtime) {
            runtimes.push(runtime);
        }
    }
    Ok(runtimes)
}

fn ask_runtimes(questions: &mut impl Questions) -> Result<Vec<Runtime>> {
    let choices = [
        "Pi, signed in to a model provider such as Anthropic or OpenAI".to_owned(),
        "T3 Code, using a T3 Code server already running on this machine".to_owned(),
    ];
    loop {
        let chosen = questions.choose_many("Which should Carillon use?", &choices, &[0])?;
        let runtimes: Vec<Runtime> = chosen
            .into_iter()
            .map(|index| if index == 0 { Runtime::Pi } else { Runtime::T3 })
            .collect();
        if !runtimes.is_empty() {
            return Ok(runtimes);
        }
        questions.say("Choose at least one.")?;
    }
}

/// The bundled runtimes an existing installation registers. A disabled one
/// is refused before anything opens: its own checks could never pass, and
/// enabling it is the operator's decision.
fn registered_runtimes(home: &Path) -> Result<Vec<Runtime>> {
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    let mut runtimes = Vec::new();
    for runtime in [Runtime::Pi, Runtime::T3] {
        let Ok(provider) = config.provider(runtime.id(), ProviderRole::Runtime) else {
            continue;
        };
        if !provider.enabled {
            let id = runtime.id();
            bail!(
                "{id} is disabled in carillon.toml; enable it with `carillon provider {id} enable`, \
                 then run setup again"
            );
        }
        runtimes.push(runtime);
    }
    if runtimes.is_empty() {
        bail!("carillon.toml registers no runtime setup sets up");
    }
    Ok(runtimes)
}

/// Opens Pi until it is signed in and has a default model, as the Pi
/// runtime's own startup check says.
fn sign_in_to_pi(home: &Path, existing: bool, questions: &mut impl Questions) -> Result<()> {
    let config = LoadedConfig::load(&home.join("carillon.toml"))?;
    if !config
        .value
        .agents
        .iter()
        .any(|agent| agent.runtime == Runtime::Pi.id())
    {
        return Ok(());
    }
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

/// Whether to keep T3's saved settings, offered only when `t3` reads them
/// and their token as its startup does.
fn keep_t3(home: &Path, t3: &Path, questions: &mut impl Questions) -> Result<bool> {
    if !home.join("providers/runtime-t3.toml").exists() {
        return Ok(false);
    }
    let config = home.join("carillon.toml");
    match run(t3, &["check", "--config", &config.to_string_lossy()], "") {
        Ok(_) => questions.confirm("Keep the T3 settings and agents?", true),
        Err(failure) => {
            questions.say(&format!(
                "The saved T3 settings cannot be kept: {failure:#}"
            ))?;
            Ok(false)
        }
    }
}

/// A T3 server, as `t3` reached it.
struct Server<'a> {
    t3: &'a Path,
    url: String,
    token: String,
}

impl Server<'_> {
    /// What the server offers now.
    fn discover(&self) -> Result<Offer> {
        let offer = run(self.t3, &["discover", "--url", &self.url], &self.token)?;
        serde_json::from_str(&offer).context("carillon-runtime-t3 discovered something unreadable")
    }

    /// `t3`'s `command` with `args` for the installation at `home`, the
    /// token on its standard input, followed by `secret` when there is one,
    /// under the maintenance lock, which no question waits behind.
    fn run(
        &self,
        home: &Path,
        command: &str,
        args: &[&str],
        secret: Option<&str>,
    ) -> Result<String> {
        let _maintenance = MaintenanceLock::acquire_unless_held(home)
            .context("the T3 settings were not changed")?;
        let config = home.join("carillon.toml");
        let config = config
            .to_str()
            .context("carillon.toml has a path that is not UTF-8")?;
        let all = [&[command, "--config", config, "--url", &self.url][..], args].concat();
        let input = match secret {
            Some(secret) => format!("{}\n{secret}", self.token),
            None => self.token.clone(),
        };
        run(self.t3, &all, &input)
    }
}

/// Asks for a T3 server and the projects T3 Code channels work in; creates
/// or extends the installation with their agents, and with Carillon's
/// `assistant` on Pi when Pi is used; then has the T3 runtime `t3` write each
/// T3 agent's settings with the default model chosen for it.
fn set_up_t3(
    home: &Path,
    options: &Options,
    assistant: bool,
    t3: &Path,
    questions: &mut impl Questions,
) -> Result<()> {
    let (server, offer) = loop {
        let url = questions.answer("T3 Code's address:", Some("http://127.0.0.1:3773"))?;
        let token = questions.secret(
            "T3 token (`t3 auth session issue --label Carillon --token-only` prints one):",
        )?;
        let server = Server { t3, url, token };
        match server.discover() {
            Ok(offer) => break (server, offer),
            Err(failure) => questions.say(&format!("{failure:#}"))?,
        }
    };
    let projects: Vec<Project> = offer
        .projects
        .into_iter()
        .filter(|project| project.workspace.is_dir())
        .collect();
    if projects.is_empty() {
        bail!("T3 has no project in a folder on this machine; add one in T3, then run setup again");
    }
    let labels: Vec<String> = projects
        .iter()
        .map(|project| format!("{} ({})", project.title, project.workspace.display()))
        .collect();
    let code: Vec<&Project> = questions
        .choose_many(
            "Which T3 projects should T3 Code channels work in? Each is an agent Discord can \
             route channels to.",
            &labels,
            &[],
        )?
        .into_iter()
        .map(|index| &projects[index])
        .collect();
    if !assistant && code.is_empty() {
        bail!("nothing runs on T3: choose a T3 Code project");
    }
    let agents = agents(home, options, assistant, Some(&code))?;
    if agents.is_empty() {
        questions.say("T3 Code has no agents to configure.")?;
        return Ok(());
    }
    configure_agents(home, &server, &agents, questions)
}

/// Has the T3 runtime write each of `agents`' settings, with its project and
/// the default model chosen for it among those the runtime offers it.
fn configure_agents(
    home: &Path,
    server: &Server<'_>,
    agents: &[(AgentConfig, &Project)],
    questions: &mut impl Questions,
) -> Result<()> {
    for (agent, project) in agents {
        let label = agent.label(agents.iter().map(|(agent, _)| agent));
        let offer = server.run(home, "discover", &["--agent", &agent.id], None)?;
        let offer: Offer = serde_json::from_str(&offer)
            .context("carillon-runtime-t3 discovered something unreadable")?;
        let models = &offer.models;
        if models.is_empty() {
            bail!("T3 offers {label} no usable model; set one up in T3, then run setup again");
        }
        let model = &models[questions.choose(
            &format!("Which model should {label} start new conversations with?"),
            &model_labels(models),
            0,
        )?];
        let model = format!("{}/{}", model.provider, model.model);
        let args = [
            "--agent",
            &agent.id,
            "--project",
            &project.id,
            "--model",
            &model,
        ];
        questions.say(server.run(home, "configure", &args, None)?.trim())?;
    }
    Ok(())
}

/// Replaces T3's agents, and with `assistant` places Carillon's assistant on
/// Pi. An existing assistant, the agent with resources, keeps its ID and the
/// stored spelling of its paths. Other runtimes' plain agents stay. `None`
/// keeps T3's plain agents; `Some` replaces them with the selected projects.
/// Returns the T3 agents, each with its project.
fn agents<'a>(
    home: &Path,
    options: &Options,
    assistant: bool,
    code: Option<&[&'a Project]>,
) -> Result<Vec<(AgentConfig, &'a Project)>> {
    let config_path = home.join("carillon.toml");
    let workspace = if assistant {
        Some(assistant_workspace(&config_path, options)?)
    } else {
        None
    };
    let runtime = if assistant { Runtime::Pi } else { Runtime::T3 };
    if !config_path.exists() {
        let agents = workspace
            .iter()
            .map(|workspace| admin::assistant(workspace));
        initialize(home, options.base_port, runtime, agents.collect())?;
    }
    let _maintenance = MaintenanceLock::acquire_unless_held(home)
        .context("the installation's agents were not changed")?;
    register_runtime(&config_path, runtime)?;
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    let projects = saved_projects(home)?;
    let mut registry: Vec<AgentConfig> = config
        .agents
        .iter()
        .filter(|agent| {
            agent.resources.is_none() && (code.is_none() || agent.runtime != Runtime::T3.id())
        })
        .cloned()
        .collect();
    if let Some(workspace) = workspace {
        let mut agent = config
            .agents
            .iter()
            .find(|agent| agent.resources.is_some())
            .cloned()
            .unwrap_or_else(|| admin::assistant(&workspace));
        Runtime::Pi.id().clone_into(&mut agent.runtime);
        agent.workspace = workspace;
        registry.push(agent);
    }
    let mut t3 = Vec::new();
    for &project in code.into_iter().flatten() {
        let id = project_agent_id(&config.agents, &registry, &projects, project);
        let agent = AgentConfig {
            id,
            name: format!("T3 Code · {}", project.title),
            runtime: Runtime::T3.id().to_owned(),
            workspace: project.workspace.clone(),
            resources: None,
        };
        registry.push(agent.clone());
        t3.push((agent, project));
    }
    if !t3.is_empty() {
        register_runtime(&config_path, Runtime::T3)?;
    }
    set_agents(&config_path, registry)?;
    Ok(t3)
}

/// The assistant's folder: the one given, or else the existing assistant's,
/// or else your home.
fn assistant_workspace(config_path: &Path, options: &Options) -> Result<PathBuf> {
    if let Some(workspace) = &options.workspace {
        return Ok(workspace.clone());
    }
    if config_path.exists() {
        let config: Config = toml::from_str(&std::fs::read_to_string(config_path)?)?;
        if let Some(agent) = config
            .agents
            .into_iter()
            .find(|agent| agent.resources.is_some())
        {
            return Ok(agent.workspace);
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME names no folder for the agent; give --workspace")
}

#[derive(Deserialize)]
struct ProjectSettings {
    project_id: String,
}

fn saved_projects(home: &Path) -> Result<BTreeMap<String, ProjectSettings>> {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default)]
        agents: BTreeMap<String, ProjectSettings>,
    }
    let path = home.join("providers/runtime-t3.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let settings: Settings =
        toml::from_str(&text).with_context(|| format!("read projects from {}", path.display()))?;
    Ok(settings.agents)
}

fn project_agent_id(
    existing: &[AgentConfig],
    registry: &[AgentConfig],
    projects: &BTreeMap<String, ProjectSettings>,
    project: &Project,
) -> String {
    if let Some(agent) = existing.iter().find(|agent| {
        agent.runtime == Runtime::T3.id()
            && agent.resources.is_none()
            && projects
                .get(&agent.id)
                .is_some_and(|saved| saved.project_id == project.id)
    }) {
        return agent.id.clone();
    }
    let taken: Vec<&str> = existing
        .iter()
        .chain(registry)
        .map(|agent| agent.id.as_str())
        .chain(projects.keys().map(String::as_str))
        .collect();
    unique(&taken, &format!("t3-{}", slug(&project.title)))
}

/// `base`, or the first of `base-2`, `base-3` and so on not in `taken`.
fn unique(taken: &[&str], base: &str) -> String {
    if !taken.contains(&base) {
        return base.to_owned();
    }
    // Among one more name than are taken, one is free.
    (2..=taken.len() + 1)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|id| !taken.contains(&id.as_str()))
        .unwrap_or_else(|| base.to_owned())
}

/// A title as an agent ID's part: lowercase ASCII letters and digits, words
/// joined by `-`.
fn slug(title: &str) -> String {
    let words: Vec<String> = title
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        "project".to_owned()
    } else {
        words.join("-").chars().take(60).collect()
    }
}

/// The model picker's labels: each model's name and T3 instance, and its
/// slug, which is what is saved, when another model of that instance shares
/// the name.
fn model_labels(models: &[AvailableModel]) -> Vec<String> {
    models
        .iter()
        .map(|model| {
            let shared = models
                .iter()
                .filter(|other| other.provider == model.provider && other.name == model.name)
                .count()
                > 1;
            if shared {
                format!("{} ({}/{})", model.name, model.provider, model.model)
            } else {
                format!("{} ({})", model.name, model.provider)
            }
        })
        .collect()
}

/// Runs `program` with `secret` on its standard input, never as an
/// argument: what it printed, or the one line it gave as its error.
fn run(program: &Path, args: &[&str], secret: &str) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", program.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A program that ends without reading still reports why.
        let _ = writeln!(stdin, "{secret}");
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("{} did not finish", program.display()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let error = String::from_utf8_lossy(&output.stderr);
    let error = error.trim();
    bail!("{}", error.strip_prefix("error: ").unwrap_or(error))
}

#[cfg(test)]
mod tests;
