use std::{os::unix::fs::PermissionsExt as _, path::Path};

use anyhow::Context as _;

use super::{
    Assistant, CATALOG, Harnesses, Shown, Unasked, fake_t3, initialize, listed, model, options,
    run, set_up_t3,
};
use crate::application::setup::{
    Runtime, Server, ask_assistant,
    profiles::{Harness, SignIn, choose_harnesses, make_profile},
};

fn listed_as(id: &str, name: &str, sign_in: &serde_json::Value) -> anyhow::Result<Harness> {
    Ok(serde_json::from_value(serde_json::json!({
        "id": id,
        "name": name,
        "instanceId": format!("carillon_{id}"),
        "signIn": sign_in,
    }))?)
}

fn native_program(directory: &Path, name: &str, exit: &str) -> anyhow::Result<String> {
    let program = directory.join(name);
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\necho \"$* CLAUDE_CONFIG_DIR=${{CLAUDE_CONFIG_DIR-unset}} \
             OPENCODE_CONFIG_DIR=${{OPENCODE_CONFIG_DIR-unset}} \
             XDG_DATA_HOME=${{XDG_DATA_HOME-unset}}\" >>'{calls}'\n{exit}\n",
            calls = directory.join(format!("{name}-calls")).display(),
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    Ok(program.to_string_lossy().into_owned())
}

fn calls(directory: &Path, name: &str) -> String {
    std::fs::read_to_string(directory.join(format!("{name}-calls"))).unwrap_or_default()
}

fn server(program: &Path) -> Server<'_> {
    Server {
        t3: program,
        url: "http://127.0.0.1:3773".to_owned(),
        token: "token".to_owned(),
    }
}

#[test]
fn the_runtime_lists_harnesses_of_both_sign_in_kinds_that_setup_reads() -> anyhow::Result<()> {
    let all = listed()?;
    let own = all
        .iter()
        .position(|harness| matches!(harness.sign_in, SignIn::OwnAccount(_)))
        .context("no harness with an account of its own")?;
    let shared = all
        .iter()
        .position(|harness| matches!(harness.sign_in, SignIn::SharedAccount(_)))
        .context("no harness sharing the user's sign-in")?;
    let directory = tempfile::tempdir()?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": []}),
    )?;
    assert_eq!(run(&program, &["harnesses"], "")?, CATALOG);
    let names: Vec<&str> = all.iter().map(|harness| harness.name.as_str()).collect();
    let mut questions = Shown::answering(&[&format!("{own},{shared}")]);
    let chosen = choose_harnesses(&program, &mut questions)?.context("two were chosen")?;
    assert_eq!(
        questions.seen,
        names
            .iter()
            .map(|name| (*name).to_owned())
            .chain(std::iter::once(
                "Carillon's assistant, with Discord's context, runs on T3 with Carillon's \
                 instructions and skills. Which should it use? Choose none for T3 Code channels \
                 only."
                    .to_owned()
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        chosen.iter().map(|harness| &harness.id).collect::<Vec<_>>(),
        [&all[own].id, &all[shared].id]
    );
    Ok(())
}

#[test]
fn any_harness_the_runtime_lists_can_be_chosen() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = directory.path().join("carillon-runtime-t3");
    let catalog = serde_json::json!([
        {"id": "zed", "name": "Zed", "instanceId": "carillon_zed", "signIn": {
            "kind": "own-account", "homeVariable": "ZED_HOME",
            "program": "zed", "login": ["login"], "status": ["whoami"],
        }},
        {"id": "acme", "name": "Acme Code", "instanceId": "carillon_acme", "signIn": {
            "kind": "shared-account", "program": "acme", "login": ["auth"],
        }},
    ]);
    std::fs::write(
        &program,
        format!("#!/bin/sh\n[ \"$1\" = harnesses ] || exit 2\nprintf '%s' '{catalog}'\n"),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    let mut questions = Shown::answering(&["1"]);
    let chosen = choose_harnesses(&program, &mut questions)?.context("Acme was chosen")?;
    assert_eq!(&questions.seen[..2], ["Zed", "Acme Code"]);
    assert_eq!(
        chosen
            .iter()
            .map(|harness| (harness.id.as_str(), harness.instance_id.as_str()))
            .collect::<Vec<_>>(),
        [("acme", "carillon_acme")]
    );
    assert_eq!(
        choose_harnesses(&program, &mut Shown::answering(&[""]))?,
        None
    );
    Ok(())
}

#[test]
fn a_catalog_setup_cannot_read_names_the_release_mismatch() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = directory.path().join("carillon-runtime-t3");
    let own = serde_json::json!({"kind": "own-account", "homeVariable": "H", "program": "p", "login": [], "status": []});
    let shared = serde_json::json!({"kind": "shared-account", "program": "p", "login": []});
    let entry = |id: &str, sign_in: &serde_json::Value| serde_json::json!({"id": id, "name": id, "instanceId": format!("carillon_{id}"), "signIn": sign_in});
    let mut with_status = shared.clone();
    with_status["status"] = serde_json::json!([]);
    let mut without_home = own.clone();
    without_home
        .as_object_mut()
        .context("object")?
        .remove("homeVariable");
    let mut unknown_kind = shared.clone();
    unknown_kind["kind"] = serde_json::json!("device-code");
    let mut other_instance = entry("b", &own);
    other_instance["instanceId"] = serde_json::json!("carillon_a");
    let mut extra_field = entry("a", &own);
    extra_field["driver"] = serde_json::json!("claudeAgent");
    for (listed, reason) in [
        ("not json".to_owned(), "listed them unreadably"),
        ("[]".to_owned(), "lists no harness"),
        (
            serde_json::json!([entry("a", &own), entry("a", &shared)]).to_string(),
            "a is listed twice",
        ),
        (
            serde_json::json!([entry("a", &own), other_instance]).to_string(),
            "b is listed twice",
        ),
        (
            serde_json::json!([entry("a", &with_status)]).to_string(),
            "listed them unreadably",
        ),
        (
            serde_json::json!([entry("a", &without_home)]).to_string(),
            "listed them unreadably",
        ),
        (
            serde_json::json!([entry("a", &unknown_kind)]).to_string(),
            "listed them unreadably",
        ),
        (
            serde_json::json!([extra_field]).to_string(),
            "listed them unreadably",
        ),
        (
            serde_json::json!([entry(" ", &own)]).to_string(),
            "has no ID, name or instance",
        ),
    ] {
        std::fs::write(
            &program,
            format!("#!/bin/sh\ncat >/dev/null\nprintf '%s' '{listed}'\n"),
        )?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        let refusal = choose_harnesses(&program, &mut Unasked)
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        assert!(
            refusal.contains("carillon-runtime-t3 and carillon disagree on the harnesses")
                && refusal.contains(reason),
            "{listed}: {refusal}"
        );
    }
    Ok(())
}

#[test]
fn a_new_shared_profile_asks_no_method_and_runs_no_login_unless_asked() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": []}),
    )?;
    let opencode = native_program(directory.path(), "opencode", "exit 0")?;
    let shared = listed_as(
        "opencode",
        "OpenCode",
        &serde_json::json!({"kind": "shared-account", "program": opencode, "login": ["auth", "login"]}),
    )?;
    let mut questions = Shown::default();
    let profile = make_profile(&home, &server(&program), &shared, &mut questions)?;
    assert_eq!(profile.instance_id, "carillon_opencode");
    assert_eq!(profile.home, directory.path().join("profiles/opencode"));
    let runtime_calls = std::fs::read_to_string(directory.path().join("calls"))?;
    assert_eq!(
        runtime_calls,
        format!(
            "discover --url http://127.0.0.1:3773\n  stdin: token\nprofile --config {} --url \
             http://127.0.0.1:3773 --harness opencode\n  stdin: token\n",
            home.join("carillon.toml").display()
        )
    );
    assert_eq!(
        questions.seen,
        [
            format!(
                "OpenCode for Carillon is T3's instance carillon_opencode, at home in {}.",
                profile.home.display()
            ),
            "OpenCode for Carillon shares your own OpenCode sign-in, and its free models need none."
                .to_owned(),
            "Sign OpenCode in to a provider now?".to_owned(),
            format!("`{opencode} auth login` adds a provider later."),
        ]
    );
    assert_eq!(calls(directory.path(), "opencode"), "");

    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": [], "profiles": ["carillon_opencode"]}),
    )?;
    let kept = make_profile(&home, &server(&program), &shared, &mut Unasked)?;
    assert_eq!(kept.instance_id, "carillon_opencode");
    assert_eq!(calls(directory.path(), "opencode"), "");
    Ok(())
}

#[test]
fn the_shared_login_runs_as_listed_and_its_failure_does_not_stop_setup() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": []}),
    )?;
    let attempts = directory.path().join("attempts");
    let opencode = native_program(
        directory.path(),
        "opencode",
        &format!(
            "[ \"$1 $2\" = 'auth login' ] || exit 3\n[ -f '{attempts}' ] && exit 0\ntouch '{attempts}'\nexit 1",
            attempts = attempts.display()
        ),
    )?;
    let shared = listed_as(
        "opencode",
        "OpenCode",
        &serde_json::json!({"kind": "shared-account", "program": opencode, "login": ["auth", "login"]}),
    )?;
    let inherited = format!(
        "CLAUDE_CONFIG_DIR={} OPENCODE_CONFIG_DIR={} XDG_DATA_HOME={}",
        std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| "unset".to_owned()),
        std::env::var("OPENCODE_CONFIG_DIR").unwrap_or_else(|_| "unset".to_owned()),
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| "unset".to_owned()),
    );

    let (sign_in, give_up) = (true, false);
    let mut questions = Shown::confirming(&[], &[sign_in, give_up]);
    make_profile(&home, &server(&program), &shared, &mut questions)?;
    assert_eq!(
        calls(directory.path(), "opencode"),
        format!("auth login {inherited}\n")
    );
    assert_eq!(
        &questions.seen[2..],
        [
            "Sign OpenCode in to a provider now?",
            "OpenCode did not finish signing in.",
            "Try again?",
            &format!("`{opencode} auth login` adds a provider later."),
        ]
    );

    std::fs::remove_file(directory.path().join("opencode-calls"))?;
    std::fs::remove_file(&attempts)?;
    let mut questions = Shown::confirming(&[], &[sign_in]);
    make_profile(&home, &server(&program), &shared, &mut questions)?;
    assert_eq!(
        calls(directory.path(), "opencode"),
        format!("auth login {inherited}\nauth login {inherited}\n")
    );
    assert_eq!(
        &questions.seen[2..],
        [
            "Sign OpenCode in to a provider now?",
            "OpenCode did not finish signing in.",
            "Try again?",
        ]
    );

    let missing = directory.path().join("missing").display().to_string();
    let absent = listed_as(
        "opencode",
        "OpenCode",
        &serde_json::json!({"kind": "shared-account", "program": missing, "login": ["auth", "login"]}),
    )?;
    let (sign_in, try_again) = (true, true);
    let mut questions = Shown::confirming(&[], &[sign_in, try_again]);
    make_profile(&home, &server(&program), &absent, &mut questions)?;
    // The system's own words for why it did not start follow the command.
    let not_started = format!("`{missing} auth login` did not start: ");
    let shown: Vec<&str> = questions.seen[2..]
        .iter()
        .map(|line| {
            if line.starts_with(&not_started)
                && line.ends_with(". Install OpenCode, then try again.")
            {
                "not started"
            } else {
                line
            }
        })
        .collect();
    assert_eq!(
        shown,
        [
            "Sign OpenCode in to a provider now?",
            "not started",
            "Try again?",
            "not started",
            "Try again?",
            &format!("`{missing} auth login` adds a provider later."),
        ]
    );
    Ok(())
}

#[test]
fn an_account_of_its_own_signs_in_through_its_program_at_the_profile_home() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": []}),
    )?;
    let session = directory.path().join("session");
    let claude = native_program(
        directory.path(),
        "claude",
        &format!(
            "case \"$1 $2\" in\n  'auth status') [ -f '{session}' ] ;;\n  'auth login') touch '{session}' ;;\n  *) exit 3 ;;\nesac",
            session = session.display()
        ),
    )?;
    let own = listed_as(
        "claude",
        "Claude",
        &serde_json::json!({"kind": "own-account", "homeVariable": "CLAUDE_CONFIG_DIR", "program": claude, "login": ["auth", "login"], "status": ["auth", "status"]}),
    )?;
    let (browser_login, open_it) = ("0", "");
    let mut questions = Shown::answering(&[browser_login, open_it]);
    let profile = make_profile(&home, &server(&program), &own, &mut questions)?;
    let at_home = format!(
        "CLAUDE_CONFIG_DIR={} OPENCODE_CONFIG_DIR={} XDG_DATA_HOME={}",
        profile.home.display(),
        std::env::var("OPENCODE_CONFIG_DIR").unwrap_or_else(|_| "unset".to_owned()),
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| "unset".to_owned()),
    );
    assert_eq!(
        calls(directory.path(), "claude"),
        format!("auth status {at_home}\nauth login {at_home}\nauth status {at_home}\n")
    );
    assert!(
        questions
            .seen
            .contains(&"Claude opens its sign-in next, for Carillon's profile alone.".to_owned()),
        "{:?}",
        questions.seen
    );

    std::fs::remove_file(directory.path().join("claude-calls"))?;
    make_profile(
        &home,
        &server(&program),
        &own,
        &mut Shown::answering(&["0"]),
    )?;
    assert_eq!(
        calls(directory.path(), "claude"),
        format!("auth status {at_home}\n")
    );

    std::fs::remove_file(&session)?;
    let claude = native_program(directory.path(), "claude", "exit 1")?;
    let own = listed_as(
        "claude",
        "Claude",
        &serde_json::json!({"kind": "own-account", "homeVariable": "CLAUDE_CONFIG_DIR", "program": claude, "login": ["auth", "login"], "status": ["auth", "status"]}),
    )?;
    let (browser_login, open_it, give_up) = ("0", "", false);
    let refusal = make_profile(
        &home,
        &server(&program),
        &own,
        &mut Shown::confirming(&[browser_login, open_it], &[give_up]),
    )
    .err()
    .map(|error| format!("{error:#}"))
    .unwrap_or_default();
    assert_eq!(
        refusal,
        "Claude is not signed in, so setup stopped here; run it again to finish"
    );
    Ok(())
}

#[test]
fn a_profile_made_for_another_instance_is_refused_before_any_sign_in() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    initialize(&home, directory.path(), 44_000, Runtime::T3)?;
    let program = directory.path().join("carillon-runtime-t3");
    std::fs::write(
        &program,
        "#!/bin/sh\ncat >/dev/null\ncase \"$1\" in\n  discover) printf '{\"projects\":[],\"models\":[],\"profiles\":[]}' ;;\n  \
         profile) printf '{\"instanceId\":\"carillon_claude\",\"home\":\"/elsewhere\"}' ;;\n  *) exit 2 ;;\nesac\n",
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    let opencode = native_program(directory.path(), "opencode", "exit 0")?;
    let shared = listed_as(
        "opencode",
        "OpenCode",
        &serde_json::json!({"kind": "shared-account", "program": opencode, "login": ["auth", "login"]}),
    )?;
    let sign_in = true;
    let refusal = make_profile(
        &home,
        &server(&program),
        &shared,
        &mut Shown::confirming(&[], &[sign_in]),
    )
    .err()
    .map(|error| format!("{error:#}"))
    .unwrap_or_default();
    assert_eq!(
        refusal,
        "carillon-runtime-t3 made profile carillon_claude for OpenCode instead of carillon_opencode"
    );
    assert_eq!(calls(directory.path(), "opencode"), "");
    Ok(())
}

#[test]
fn the_assistant_question_offers_the_harnesses_the_runtime_lists() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": []}),
    )?;
    let names: Vec<String> = listed()?.into_iter().map(|harness| harness.name).collect();
    let mut questions = Shown::answering(&["1", "0"]);
    let chosen = ask_assistant(true, &program, &mut questions)?;
    assert_eq!(
        questions.seen[..3],
        [
            "Pi".to_owned(),
            "T3 Code, with a Carillon profile".to_owned(),
            "What should run Carillon's assistant?".to_owned(),
        ]
    );
    assert_eq!(questions.seen[3..3 + names.len()], names);
    let Assistant::T3(harnesses) = chosen else {
        anyhow::bail!("T3 was chosen");
    };
    assert_eq!(
        harnesses
            .iter()
            .map(|harness| &harness.name)
            .collect::<Vec<_>>(),
        [&names[0]]
    );
    let mut questions = Shown::answering(&["0"]);
    assert_eq!(
        ask_assistant(true, &program, &mut questions)?,
        Assistant::Pi
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("calls"))?,
        "harnesses\n  stdin: \n"
    );
    let mut questions = Shown::answering(&[""]);
    assert_eq!(
        ask_assistant(false, &program, &mut questions)?,
        Assistant::None
    );
    assert_eq!(questions.seen[..names.len()], names);
    Ok(())
}

#[test]
fn t3_gets_the_assistant_on_opencode_with_no_sign_in() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let offer = serde_json::json!({
        "projects": [{"id": "p-assistant", "title": "Carillon", "workspace": workspace}],
        "models": [
            model("opencode", "deepseek-free", "DeepSeek (free)"),
            model("carillon_opencode", "deepseek-free", "DeepSeek (free)"),
        ],
        "agentModels": {"assistant": [model("carillon_opencode", "deepseek-free", "DeepSeek (free)")]},
    });
    let program = fake_t3(directory.path(), &offer)?;
    let opencode = native_program(directory.path(), "opencode", "exit 0")?;
    let shared = listed_as(
        "opencode",
        "OpenCode",
        &serde_json::json!({"kind": "shared-account", "program": opencode, "login": ["auth", "login"]}),
    )?;
    let (default_address, token, assistant_project, no_code_project, first_model) =
        ("", "token", "0", "", "0");
    let mut questions = Shown::answering(&[
        default_address,
        token,
        assistant_project,
        no_code_project,
        first_model,
    ]);
    set_up_t3(
        &home,
        &options(),
        &Assistant::T3(Harnesses::chosen(vec![shared], &[0]).context("OpenCode was chosen")?),
        &program,
        &mut questions,
    )?;
    assert_eq!(calls(directory.path(), "opencode"), "");
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    let config_arg = home.join("carillon.toml");
    let config_arg = config_arg.display();
    for expected in [
        format!(
            "profile --config {config_arg} --url http://127.0.0.1:3773 --harness opencode\n  stdin: token\n"
        ),
        format!(
            "configure --config {config_arg} --url http://127.0.0.1:3773 --agent assistant \
             --project p-assistant --model carillon_opencode/deepseek-free\n"
        ),
    ] {
        assert!(calls.contains(&expected), "{expected}\nnot in\n{calls}");
    }
    assert!(!calls.contains("--with-key"), "{calls}");
    assert!(
        questions.seen.contains(
            &"OpenCode for Carillon shares your own OpenCode sign-in, and its free models need none."
                .to_owned()
        ),
        "{:?}",
        questions.seen
    );
    assert!(
        !questions
            .seen
            .iter()
            .any(|shown| shown.starts_with("How should")),
        "{:?}",
        questions.seen
    );
    Ok(())
}
