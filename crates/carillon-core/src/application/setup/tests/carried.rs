use std::path::Path;

use carillon_installation::config::RELEASE_VERSION;

use super::{
    Runtime, SavedT3, Unasked, fake_t3, initialized, model, options, set_up_t3, start_fresh,
};
use crate::application::setup::{Project, agents};

const PREVIOUS: [&str; 3] = ["0.0.1-previous"; 3];

fn bundled() -> anyhow::Result<[String; 3]> {
    Ok([
        RELEASE_VERSION.to_owned(),
        Runtime::Pi.version()?,
        Runtime::T3.version()?,
    ])
}

fn carried(workspace: &Path, [discord, pi, t3]: [&str; 3]) -> String {
    let hash = "0".repeat(64);
    let workspace = workspace.display();
    format!(
        r#"# Owner note: core listens on the base port.
data_dir = 'state' # state location

[core]
listen = "127.0.0.1:44000"
operator_token_hash = "{hash}"

[[providers]]
id = "discord-private"
role = "interface"
version  =  '{discord}' # owner version note
url = "http://127.0.0.1:44001"
token_hash = "{hash}"
enabled = true
max_upload_mib = 25

[[providers]]
id = "runtime-pi"
role = "runtime"
version = "{pi}"
url = "http://127.0.0.1:44002"
token_hash = "{hash}"
enabled = true

[[providers]]
id = "runtime-t3"
role = "runtime"
version = "{t3}"
url = "http://127.0.0.1:44003"
token_hash = "{hash}"
enabled = true

[[providers]]
id = "custom-runtime"
role = "runtime"
version = "9.8.7" # custom release
url = "http://127.0.0.1:44004"
token_hash = "{hash}"
enabled = false

# The assistant.
[[agents]]
id = "assistant"
name = "Carillon"
runtime = "runtime-pi"
workspace = "{workspace}"
resources = "agents/default"

# Notes, on T3.
[[agents]]
id = "t3-notes"
name = "T3 Code · Notes"
runtime = "runtime-t3"
workspace = "{workspace}"
"#
    )
}

fn carried_inline(workspace: &Path, [discord, pi, _]: [&str; 3]) -> String {
    let hash = "0".repeat(64);
    let workspace = workspace.display();
    format!(
        r#"providers = [
  {{ id = "discord-private", role = "interface", version = '{discord}', url = "http://127.0.0.1:44001", token_hash = "{hash}", enabled = true }}, # the bot
  {{ id = "runtime-pi", role = "runtime", version = "{pi}", url = "http://127.0.0.1:44002", token_hash = "{hash}", enabled = true }},
  {{ id = "custom-runtime", role = "runtime", version = "9.8.7", url = "http://127.0.0.1:44004", token_hash = "{hash}", enabled = false }},
]
data_dir = "state"

[core]
listen = "127.0.0.1:44000"
operator_token_hash = "{hash}"

[[agents]]
id = "assistant"
name = "Carillon"
runtime = "runtime-pi"
workspace = "{workspace}"
resources = "agents/default"
"#
    )
}

fn carry(home: &Path, workspace: &Path, registry: &str) -> anyhow::Result<std::path::PathBuf> {
    let path = initialized(home, workspace, Runtime::Pi)?;
    std::fs::write(&path, registry)?;
    std::fs::remove_file(home.join("state/carillon.sqlite3"))?;
    Ok(path)
}

#[test]
fn a_carried_registry_gets_this_releases_versions_and_keeps_the_rest_as_written()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let bundled = bundled()?;
    let bundled = bundled.each_ref().map(String::as_str);
    let forms: [fn(&Path, [&str; 3]) -> String; 2] = [carried, carried_inline];
    for (index, form) in forms.into_iter().enumerate() {
        for ending in ["\n", "\r\n"] {
            let home = directory
                .path()
                .join(format!("home-{index}-{}", ending.len()));
            let written = form(directory.path(), PREVIOUS).replace('\n', ending);
            let path = carry(&home, directory.path(), &written)?;
            start_fresh(&home)?;
            assert_eq!(
                std::fs::read_to_string(&path)?,
                form(directory.path(), bundled).replace('\n', ending)
            );
            crate::db::Database::open(&home.join("state/carillon.sqlite3"))?;
        }
    }
    Ok(())
}

#[test]
fn setup_leaves_a_carried_registry_as_written_but_for_what_it_adds() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let written = carried(directory.path(), PREVIOUS);
    let path = carry(&home, directory.path(), &written)?;
    std::fs::write(
        home.join("providers/runtime-t3.toml"),
        "url = 'http://127.0.0.1:3999'\ntoken_file = 'credentials/t3.token'\n\
         [agents.t3-notes]\nproject_id = 'notes'\n",
    )?;
    agents(&home, &options(), true, None)?;
    assert_eq!(std::fs::read_to_string(&path)?, written);
    let project = |id: &str, title: &str| Project {
        id: id.to_owned(),
        title: title.to_owned(),
        workspace: directory.path().to_path_buf(),
    };
    let notes = project("notes", "Notes");
    let code = project("code", "Code");
    agents(&home, &options(), true, Some(&[&notes, &code]))?;
    let added = format!(
        "\n[[agents]]\nid = \"t3-code\"\nname = \"T3 Code · Code\"\nruntime = \"runtime-t3\"\n\
         workspace = \"{}\"\n",
        directory.path().display()
    );
    assert_eq!(std::fs::read_to_string(&path)?, format!("{written}{added}"));
    Ok(())
}

#[test]
fn kept_t3_settings_need_no_t3_when_every_agent_has_a_model() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    carry(
        &home,
        directory.path(),
        &carried(directory.path(), PREVIOUS),
    )?;
    std::fs::write(home.join("credentials/t3.token"), "saved-token\n")?;
    std::fs::write(
        home.join("providers/runtime-t3.toml"),
        "url = 'http://127.0.0.1:3999'\ntoken_file = 'credentials/t3.token'\n\
         [agents.t3-notes]\nproject_id = 'notes'\n",
    )?;
    let program = fake_t3(
        directory.path(),
        &serde_json::json!({"projects": [], "models": [model("codex", "gpt", "GPT")]}),
    )?;
    std::fs::write(directory.path().join("unreachable"), "")?;
    std::fs::write(directory.path().join("model-t3-notes"), "codex/gpt")?;
    set_up_t3(
        &home,
        &options(),
        true,
        &program,
        SavedT3::Kept,
        &mut Unasked,
    )?;
    let calls = std::fs::read_to_string(directory.path().join("calls"))?;
    assert_eq!(
        calls,
        format!(
            "model --config {} --agent t3-notes\n  stdin: \n",
            home.join("carillon.toml").display()
        )
    );
    Ok(())
}
