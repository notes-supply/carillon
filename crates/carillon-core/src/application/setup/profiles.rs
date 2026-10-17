use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, bail, ensure};
use carillon_installation::prompt::Questions;
use serde::Deserialize;

use super::{Server, run};

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Harness {
    /// As `carillon-runtime-t3 profile --harness` names it.
    pub(super) id: String,
    pub(super) name: String,
    pub(super) instance_id: String,
    pub(super) sign_in: SignIn,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum SignIn {
    OwnAccount(OwnAccount),
    SharedAccount(SharedAccount),
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OwnAccount {
    /// The variable naming the home the sign-in is kept in.
    home_variable: String,
    program: String,
    login: Vec<String>,
    status: Vec<String>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SharedAccount {
    program: String,
    login: Vec<String>,
}

/// The harnesses the assistant runs on, of which there is at least one.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Harnesses {
    first: Harness,
    rest: Vec<Harness>,
}

impl Harnesses {
    pub(super) fn chosen(all: Vec<Harness>, indexes: &[usize]) -> Option<Self> {
        let mut chosen = all
            .into_iter()
            .enumerate()
            .filter(|(index, _)| indexes.contains(index))
            .map(|(_, harness)| harness);
        Some(Self {
            first: chosen.next()?,
            rest: chosen.collect(),
        })
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Harness> {
        std::iter::once(&self.first).chain(&self.rest)
    }
}

/// A profile `carillon-runtime-t3 profile` made.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Profile {
    pub(super) instance_id: String,
    pub(super) home: PathBuf,
}

pub(super) fn choose_harnesses(
    t3: &Path,
    questions: &mut impl Questions,
) -> Result<Option<Harnesses>> {
    let all = catalog(t3)?;
    let names: Vec<String> = all.iter().map(|harness| harness.name.clone()).collect();
    let chosen = questions.choose_many(
        "Carillon's assistant, with Discord's context, runs on T3 with Carillon's instructions \
         and skills. Which should it use? Choose none for T3 Code channels only.",
        &names,
        &[0],
    )?;
    Ok(Harnesses::chosen(all, &chosen))
}

fn catalog(t3: &Path) -> Result<Vec<Harness>> {
    let mismatch = "carillon-runtime-t3 and carillon disagree on the harnesses; install the same \
                    release of both";
    let listed = run(t3, &["harnesses"], "")?;
    let all: Vec<Harness> = serde_json::from_str(&listed)
        .with_context(|| format!("{mismatch}: carillon-runtime-t3 listed them unreadably"))?;
    ensure!(
        !all.is_empty(),
        "{mismatch}: carillon-runtime-t3 lists no harness"
    );
    for (index, harness) in all.iter().enumerate() {
        ensure!(
            !harness.id.trim().is_empty()
                && !harness.name.trim().is_empty()
                && !harness.instance_id.trim().is_empty(),
            "{mismatch}: a listed harness has no ID, name or instance"
        );
        ensure!(
            all[..index].iter().all(|earlier| {
                earlier.id != harness.id && earlier.instance_id != harness.instance_id
            }),
            "{mismatch}: {} is listed twice",
            harness.id
        );
    }
    Ok(all)
}

enum Plan<'a> {
    Kept,
    Login(&'a OwnAccount),
    Key(String),
    Endpoint { address: String, token: String },
    Shared(&'a SharedAccount),
}

fn ask_method<'a>(
    name: &str,
    account: &'a OwnAccount,
    questions: &mut impl Questions,
) -> Result<Plan<'a>> {
    let choices = [
        format!("With a {name} account, in the browser"),
        "With an API key".to_owned(),
        "With a compatible endpoint's address and token".to_owned(),
    ];
    Ok(
        match questions.choose(
            &format!("How should {name} sign in for Carillon?"),
            &choices,
            0,
        )? {
            0 => Plan::Login(account),
            1 => Plan::Key(questions.secret(&format!("{name} API key:"))?),
            _ => Plan::Endpoint {
                address: questions.answer("The endpoint's address:", None)?,
                token: questions.secret("The endpoint's token:")?,
            },
        },
    )
}

pub(super) fn make_profile(
    home: &Path,
    server: &Server<'_>,
    harness: &Harness,
    questions: &mut impl Questions,
) -> Result<Profile> {
    let name = &harness.name;
    let plan = if server
        .discover(&[])?
        .profiles
        .contains(&harness.instance_id)
    {
        Plan::Kept
    } else {
        match &harness.sign_in {
            SignIn::OwnAccount(account) => ask_method(name, account, questions)?,
            SignIn::SharedAccount(shared) => Plan::Shared(shared),
        }
    };
    let mut args = vec!["--harness", harness.id.as_str()];
    let key = match &plan {
        Plan::Key(key) => Some(key.as_str()),
        Plan::Endpoint { token, .. } => Some(token.as_str()),
        Plan::Kept | Plan::Login(_) | Plan::Shared(_) => None,
    };
    if key.is_some() {
        args.push("--with-key");
    }
    if let Plan::Endpoint { address, .. } = &plan {
        args.extend(["--endpoint", address]);
    }
    let made = server.run(home, "profile", &args, key)?;
    let profile: Profile =
        serde_json::from_str(&made).context("read the profile carillon-runtime-t3 made")?;
    ensure!(
        profile.instance_id == harness.instance_id,
        "carillon-runtime-t3 made profile {} for {name} instead of {}",
        profile.instance_id,
        harness.instance_id
    );
    questions.say(&format!(
        "{name} for Carillon is T3's instance {}, at home in {}.",
        profile.instance_id,
        profile.home.display()
    ))?;
    match plan {
        Plan::Login(account) => sign_in_owned(name, account, &profile.home, questions)?,
        Plan::Shared(shared) => sign_in_shared(name, shared, questions)?,
        Plan::Kept | Plan::Key(_) | Plan::Endpoint { .. } => {}
    }
    Ok(profile)
}

/// Runs the harness's own sign-in into its profile's home until it says it
/// is signed in.
fn sign_in_owned(
    name: &str,
    account: &OwnAccount,
    home: &Path,
    questions: &mut impl Questions,
) -> Result<()> {
    let signed_in = || -> Result<bool> {
        Ok(Command::new(&account.program)
            .args(&account.status)
            .env(&account.home_variable, home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("run {}; install {name} first", account.program))?
            .success())
    };
    if signed_in()? {
        return Ok(());
    }
    loop {
        questions.say(&format!(
            "{name} opens its sign-in next, for Carillon's profile alone."
        ))?;
        questions.answer("Press Enter to sign in.", Some(""))?;
        // The harness's own sign-in owns the terminal until it ends.
        Command::new(&account.program)
            .args(&account.login)
            .env(&account.home_variable, home)
            .status()
            .with_context(|| format!("run {}; install {name} first", account.program))?;
        if signed_in()? {
            return Ok(());
        }
        if !questions.confirm(&format!("{name} is not signed in. Try again?"), true)? {
            bail!("{name} is not signed in, so setup stopped here; run it again to finish");
        }
    }
}

/// Offers the setup user's own sign-in to the harness, which every profile
/// of it shares. Its free models need none, so declining, or a sign-in that
/// does not start or finish, leaves setup to go on; `ready` then has T3
/// list whatever models the sign-in added.
fn sign_in_shared(
    name: &str,
    shared: &SharedAccount,
    questions: &mut impl Questions,
) -> Result<()> {
    let login = std::iter::once(&shared.program)
        .chain(&shared.login)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    questions.say(&format!(
        "{name} for Carillon shares your own {name} sign-in, and its free models need none."
    ))?;
    let mut signing_in = questions.confirm(&format!("Sign {name} in to a provider now?"), false)?;
    while signing_in {
        let started = match Command::new(&shared.program).args(&shared.login).status() {
            Ok(finished) if finished.success() => return Ok(()),
            Ok(_) => {
                questions.say(&format!("{name} did not finish signing in."))?;
                true
            }
            Err(error) => {
                questions.say(&format!(
                    "`{login}` did not start: {error}. Install {name}, then try again."
                ))?;
                false
            }
        };
        signing_in = questions.confirm("Try again?", started)?;
    }
    questions.say(&format!("`{login}` adds a provider later."))?;
    Ok(())
}
