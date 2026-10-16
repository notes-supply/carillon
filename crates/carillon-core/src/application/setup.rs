//! `carillon setup`: an installation guided at the terminal. It asks which
//! runtimes Carillon uses and what runs its assistant, has each runtime set
//! up by its own program, with Carillon's own T3 profiles and the T3 projects
//! T3 Code channels work in, then has Discord's program ask for the bot and
//! who answers in which channel. Each provider's program owns its part;
//! setup only asks the questions that choose between them and runs them in
//! order. A rerun keeps what is already set up when asked to.

use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
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
    admin::{Runtime, assistant, initialize, register_runtime, set_agents},
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
    profiles: Vec<String>,
}

#[derive(Clone, Deserialize)]
struct Project {
    id: String,
    title: String,
    workspace: PathBuf,
}

/// A profile `carillon-runtime-t3 profile` made.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    instance_id: String,
    home: PathBuf,
}

/// A harness Carillon's assistant can run on T3 with a profile of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Harness {
    Claude,
    Codex,
}

/// A harness's own program, whose sign-in finishes in a browser on any
/// device, as on a headless machine.
struct Program {
    command: &'static str,
    /// The variable naming the home the sign-in is kept in.
    home: &'static str,
    login: &'static [&'static str],
    /// Its check that it is signed in.
    status: &'static [&'static str],
}

impl Harness {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// As `carillon-runtime-t3 profile --harness` names it.
    const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    const fn program(self) -> Program {
        match self {
            Self::Claude => Program {
                command: "claude",
                home: "CLAUDE_CONFIG_DIR",
                login: &["auth", "login"],
                status: &["auth", "status"],
            },
            Self::Codex => Program {
                command: "codex",
                home: "CODEX_HOME",
                login: &["login", "--device-auth"],
                status: &["login", "status"],
            },
        }
    }
}

/// The harnesses the assistant runs on, of which there is at least one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Harnesses {
    first: Harness,
    rest: Vec<Harness>,
}

impl Harnesses {
    /// Those chosen among [`Harness::ALL`], by index; none chosen is no
    /// assistant on T3.
    fn chosen(indexes: &[usize]) -> Option<Self> {
        let mut chosen = indexes
            .iter()
            .filter_map(|&index| Harness::ALL.get(index).copied());
        Some(Self {
            first: chosen.next()?,
            rest: chosen.collect(),
        })
    }

    fn iter(&self) -> impl Iterator<Item = Harness> + '_ {
        std::iter::once(self.first).chain(self.rest.iter().copied())
    }
}

/// What runs Carillon's assistant, the agent with Discord's context and
/// Carillon's own resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assistant<T = Harnesses> {
    Pi,
    /// On T3, with a profile for each harness.
    T3(T),
    /// Channels are only ever plain T3 Code.
    None,
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
    let pi = runtimes.contains(&Runtime::Pi);
    let t3 = runtimes.contains(&Runtime::T3);
    if options.workspace.is_some() && !pi {
        bail!("--workspace is for Pi; on T3, an agent works in its project's folder");
    }
    let kept = existing && t3 && keep_t3(home, &provider_binary("runtime-t3")?, questions)?;
    if kept {
        let config = LoadedConfig::load(&home.join("carillon.toml"))?;
        if options.runtimes.contains(&Runtime::Pi)
            && !config
                .value
                .agents
                .iter()
                .any(|agent| agent.runtime == Runtime::Pi.id())
        {
            questions
                .say("Pi will run Carillon's assistant. T3 Code agents keep their settings.")?;
            agents(home, options, Assistant::Pi, None)?;
        }
    } else {
        let assistant = ask_assistant(&runtimes, questions)?;
        if t3 {
            set_up_t3(
                home,
                options,
                &assistant,
                &provider_binary("runtime-t3")?,
                questions,
            )?;
        } else {
            agents(home, options, Assistant::Pi, None)?;
        }
    }
    {
        let _maintenance = MaintenanceLock::acquire_unless_held(home)?;
        for &runtime in &runtimes {
            register_runtime(&home.join("carillon.toml"), runtime)?;
        }
        let config = LoadedConfig::load(&home.join("carillon.toml"))?;
        for runtime in runtimes {
            if !config
                .value
                .agents
                .iter()
                .any(|agent| agent.runtime == runtime.id())
            {
                bail!(
                    "{} has no agent; run setup again and choose an agent for every registered runtime",
                    runtime.id()
                );
            }
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

/// What runs the assistant. Pi alone runs it; with T3 there is a choice of
/// harnesses for it, or none, when channels are only ever T3 Code.
fn ask_assistant(runtimes: &[Runtime], questions: &mut impl Questions) -> Result<Assistant> {
    let pi = runtimes.contains(&Runtime::Pi);
    if !runtimes.contains(&Runtime::T3) {
        return Ok(Assistant::Pi);
    }
    if pi {
        let choices = [
            "Pi".to_owned(),
            "T3 Code, with Carillon's own Claude or Codex".to_owned(),
        ];
        if questions.choose("What should run Carillon's assistant?", &choices, 0)? == 0 {
            return Ok(Assistant::Pi);
        }
    }
    let names: Vec<String> = Harness::ALL
        .iter()
        .map(|harness| harness.name().to_owned())
        .collect();
    let chosen = questions.choose_many(
        "Carillon's assistant, with Discord's context, runs on T3 with a sign-in of its own. \
         Which should it use? Choose none for T3 Code channels only.",
        &names,
        &[0],
    )?;
    Ok(Harnesses::chosen(&chosen).map_or(Assistant::None, Assistant::T3))
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
    /// What the server offers now, once T3 has checked the `refresh`
    /// instances again.
    fn discover(&self, refresh: &[&str]) -> Result<Offer> {
        let refresh = refresh.join(",");
        let mut args = vec!["discover", "--url", &self.url];
        if !refresh.is_empty() {
            args.extend(["--refresh", &refresh]);
        }
        let offer = run(self.t3, &args, &self.token)?;
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

/// Asks for a T3 server, the project Carillon's assistant works in when it
/// runs there, and the projects T3 Code channels work in; creates or extends
/// the installation with their agents; makes and signs in the assistant's
/// profiles; then has the T3 runtime `t3` write each agent's settings with
/// the default model chosen for it.
fn set_up_t3(
    home: &Path,
    options: &Options,
    assistant: &Assistant,
    t3: &Path,
    questions: &mut impl Questions,
) -> Result<()> {
    let (server, offer) = loop {
        let url = questions.answer("T3 Code's address:", Some("http://127.0.0.1:3773"))?;
        let token = questions.secret(
            "T3 token (`t3 auth session issue --label Carillon --token-only` prints one):",
        )?;
        let server = Server { t3, url, token };
        match server.discover(&[]) {
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
    let plan = match assistant {
        Assistant::T3(_) => Assistant::T3(
            &projects[questions.choose(
                "Which T3 project should Carillon's assistant work in?",
                &labels,
                0,
            )?],
        ),
        Assistant::Pi => Assistant::Pi,
        Assistant::None => Assistant::None,
    };
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
    if matches!(plan, Assistant::None) && code.is_empty() {
        bail!("nothing runs on T3: choose a harness for the assistant, or a T3 Code project");
    }
    let agents = agents(home, options, plan, Some(&code))?;
    if agents.is_empty() {
        questions.say("T3 Code has no agents to configure.")?;
        return Ok(());
    }
    let profiles = match assistant {
        Assistant::T3(harnesses) => harnesses
            .iter()
            .map(|harness| make_profile(home, &server, harness, questions))
            .collect::<Result<Vec<_>>>()?,
        Assistant::Pi | Assistant::None => Vec::new(),
    };
    ready(&server, &profiles, questions)?;
    configure_agents(home, &server, &agents, questions)
}

/// Has the T3 runtime write each of `agents`' settings, with its project and
/// the default model chosen for it: the assistant's among its profiles', the
/// others' among every other instance's.
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

/// Replaces T3's agents and places the assistant on the chosen runtime.
/// Existing resource paths keep their stored spelling and identify the
/// assistant even when its ID was changed. Other runtimes' plain agents stay.
/// `None` keeps T3's plain agents; `Some` replaces them with the selected projects.
fn agents<'a>(
    home: &Path,
    options: &Options,
    plan: Assistant<&'a Project>,
    code: Option<&[&'a Project]>,
) -> Result<Vec<(AgentConfig, &'a Project)>> {
    let config_path = home.join("carillon.toml");
    let workspace = match &plan {
        Assistant::Pi => options.workspace.clone().map_or_else(
            || {
                if config_path.exists() {
                    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
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
            },
            Ok,
        )?,
        Assistant::T3(project) => project.workspace.clone(),
        Assistant::None => code
            .and_then(|code| code.first())
            .context("no agent to create the installation for")?
            .workspace
            .clone(),
    };
    let runtime = if matches!(plan, Assistant::Pi) {
        Runtime::Pi
    } else {
        Runtime::T3
    };
    if !config_path.exists() {
        initialize(home, &workspace, options.base_port, runtime)?;
    }
    let _maintenance = MaintenanceLock::acquire_unless_held(home)
        .context("the installation's agents were not changed")?;
    register_runtime(&config_path, runtime)?;
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    let projects = saved_projects(home)?;
    let existing = config.agents.iter().find(|agent| agent.resources.is_some());
    let mut registry: Vec<AgentConfig> = config
        .agents
        .iter()
        .filter(|agent| {
            agent.resources.is_none() && (code.is_none() || agent.runtime != Runtime::T3.id())
        })
        .cloned()
        .collect();
    let mut t3 = Vec::new();
    if !matches!(plan, Assistant::None) {
        let mut agent = existing
            .cloned()
            .unwrap_or_else(|| assistant(runtime, &workspace));
        runtime.id().clone_into(&mut agent.runtime);
        agent.workspace = workspace;
        if let Assistant::T3(project) = plan {
            t3.push((agent.clone(), project));
        }
        registry.push(agent);
    }
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

/// Makes the assistant's profile for `harness` and signs it in, with the
/// harness's own login in its home or with a key, as asked.
fn make_profile(
    home: &Path,
    server: &Server<'_>,
    harness: Harness,
    questions: &mut impl Questions,
) -> Result<Profile> {
    let name = harness.name();
    let choices = [
        format!("With a {name} account, in the browser"),
        "With an API key".to_owned(),
        "With a compatible endpoint's address and token".to_owned(),
    ];
    let existing = server
        .discover(&[])?
        .profiles
        .contains(&format!("carillon_{}", harness.id()));
    let method = if existing {
        None
    } else {
        Some(questions.choose(
            &format!("How should {name} sign in for Carillon?"),
            &choices,
            0,
        )?)
    };
    let mut args = vec!["--harness", harness.id()];
    let (endpoint, key) = match method {
        None | Some(0) => (None, None),
        Some(1) => (None, Some(questions.secret(&format!("{name} API key:"))?)),
        Some(_) => (
            Some(questions.answer("The endpoint's address:", None)?),
            Some(questions.secret("The endpoint's token:")?),
        ),
    };
    if key.is_some() {
        args.push("--with-key");
    }
    if let Some(endpoint) = &endpoint {
        args.extend(["--endpoint", endpoint]);
    }
    let made = server.run(home, "profile", &args, key.as_deref())?;
    let profile: Profile =
        serde_json::from_str(&made).context("read the profile carillon-runtime-t3 made")?;
    questions.say(&format!(
        "{} for Carillon is T3's instance {}, at home in {}.",
        name,
        profile.instance_id,
        profile.home.display()
    ))?;
    if method == Some(0) {
        sign_in(harness, &profile.home, questions)?;
    }
    Ok(profile)
}

/// Runs the harness's own sign-in into its profile's home until it says it
/// is signed in.
fn sign_in(harness: Harness, home: &Path, questions: &mut impl Questions) -> Result<()> {
    let program = harness.program();
    let signed_in = || -> Result<bool> {
        Ok(Command::new(program.command)
            .args(program.status)
            .env(program.home, home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run {}; install {} first", program.command, harness.name()))?
            .success())
    };
    if signed_in()? {
        return Ok(());
    }
    loop {
        questions.say(&format!(
            "{} opens its sign-in next, for Carillon's profile alone.",
            harness.name()
        ))?;
        questions.answer("Press Enter to sign in.", Some(""))?;
        // The harness's own sign-in owns the terminal until it ends.
        Command::new(program.command)
            .args(program.login)
            .env(program.home, home)
            .status()
            .with_context(|| {
                format!("run {}; install {} first", program.command, harness.name())
            })?;
        if signed_in()? {
            return Ok(());
        }
        if !questions.confirm(
            &format!("{} is not signed in. Try again?", harness.name()),
            true,
        )? {
            bail!(
                "{} is not signed in, so setup stopped here; run it again to finish",
                harness.name()
            );
        }
    }
}

/// What the server offers once it lists every profile as usable. T3 checks
/// a new instance on its own time, and keeps what it found of its sign-in, so
/// each round first has it check the profiles again.
fn ready(server: &Server<'_>, profiles: &[Profile], questions: &mut impl Questions) -> Result<()> {
    let instances: Vec<&str> = profiles
        .iter()
        .map(|profile| profile.instance_id.as_str())
        .collect();
    loop {
        for attempt in 0..30 {
            let refresh = if attempt == 0 {
                instances.as_slice()
            } else {
                &[]
            };
            let offer = server.discover(refresh)?;
            if profiles.iter().all(|profile| {
                offer
                    .models
                    .iter()
                    .any(|model| model.provider == profile.instance_id)
            }) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        if !questions.confirm(
            "T3 does not list Carillon's profiles as ready yet; its provider settings say why. \
             Check again?",
            true,
        )? {
            bail!("T3 has not made Carillon's profiles ready, so setup stopped here");
        }
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
