use super::*;

#[test]
fn provider_command_rejects_unsafe_identity_before_mutation() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
    let before = fs::read(&config_path)?;
    crate::assert_refused(
        set_provider(&config_path, "../escape", false),
        "provider identity must be 1-128 bytes",
    )?;
    assert_eq!(fs::read(&config_path)?, before);
    Ok(())
}

/// Enabling, disabling and registering a provider change only their own
/// lines: the owner's notes and line endings stay, and a registered runtime
/// follows the providers before it.
#[test]
fn provider_commands_change_only_their_own_lines() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
    let generated = fs::read_to_string(&config_path)?;
    let written = format!("# Owner note.\n{generated}")
        .replacen("enabled = true\n", "enabled = true # the bot\n", 1)
        .replace('\n', "\r\n");
    fs::write(&config_path, &written)?;
    set_provider(&config_path, "runtime-pi", false)?;
    let pi = "id = \"runtime-pi\"\r\n";
    let (head, tail) = written.split_at(written.find(pi).context("runtime-pi")? + pi.len());
    let disabled = format!("{head}{}", tail.replacen("enabled = true", "enabled = false", 1));
    assert_eq!(fs::read_to_string(&config_path)?, disabled);
    super::super::register_runtime(&config_path, Runtime::T3)?;
    let config = LoadedConfig::load(&config_path)?;
    let t3 = config.provider("runtime-t3", ProviderRole::Runtime)?;
    let (providers, agents) = disabled.split_at(disabled.find("\r\n[[agents]]").context("agents")?);
    let registered = format!(
        "{providers}\r\n[[providers]]\r\nid = \"runtime-t3\"\r\nrole = \"runtime\"\r\n\
         version = \"{}\"\r\nurl = \"http://127.0.0.1:44003\"\r\ntoken_hash = \"{}\"\r\n\
         enabled = true\r\n{agents}",
        t3.version, t3.token_hash
    );
    assert_eq!(fs::read_to_string(&config_path)?, registered);
    Ok(())
}
