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
    let disabled = format!(
        "{head}{}",
        tail.replacen("enabled = true", "enabled = false", 1)
    );
    assert_eq!(fs::read_to_string(&config_path)?, disabled);
    super::super::register(&config_path, Bundled::T3)?;
    let config = LoadedConfig::load(&config_path)?;
    let t3 = config.provider("runtime-t3", ProviderRole::Runtime)?;
    let (providers, agents) = disabled.split_at(disabled.find("\r\n[[agents]]").context("agents")?);
    let registered = format!(
        "{providers}\r\n[[providers]]\r\nid = \"runtime-t3\"\r\nrole = \"runtime\"\r\n\
         program = \"runtime-t3\"\r\nversion = \"{}\"\r\nurl = \"http://127.0.0.1:44003\"\r\ntoken_hash = \"{}\"\r\n\
         enabled = true\r\n{agents}",
        t3.version, t3.token_hash
    );
    assert_eq!(fs::read_to_string(&config_path)?, registered);
    Ok(())
}

#[test]
fn configuration_edits_preserve_unrelated_temporary_siblings() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config = initialize_test_installation(&root, directory.path(), 44_000)?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o640))?;
    let sibling = config.with_extension("tmp");
    let external = directory.path().join("unrelated");
    fs::write(&external, "owner data")?;
    for linked in [false, true] {
        if linked {
            symlink(&external, &sibling)?;
        } else {
            fs::write(&sibling, "owner data")?;
        }
        set_provider(&config, "runtime-pi", false)?;
        let loaded = LoadedConfig::load(&config)?;
        assert!(
            !loaded
                .provider("runtime-pi", ProviderRole::Runtime)?
                .enabled
        );
        set_provider(&config, "runtime-pi", true)?;
        assert!(
            LoadedConfig::load(&config)?
                .provider("runtime-pi", ProviderRole::Runtime)?
                .enabled
        );
        assert_eq!(fs::read_to_string(&sibling)?, "owner data");
        assert_eq!(fs::read_to_string(&external)?, "owner data");
        assert_eq!(fs::symlink_metadata(&sibling)?.is_symlink(), linked);
        assert!(!fs::symlink_metadata(&config)?.is_symlink());
        assert_eq!(fs::metadata(&config)?.permissions().mode() & 0o777, 0o640);
        fs::remove_file(&sibling)?;
    }
    Ok(())
}

#[test]
fn an_installation_without_an_interface_is_refused_before_anything_is_written() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    crate::assert_refused(
        initialize(
            &root,
            44_000,
            &[Bundled::Pi],
            vec![assistant(directory.path())],
        ),
        "an installation needs an interface; name one with --interface",
    )?;
    assert!(!root.exists());
    Ok(())
}

fn registered(config_path: &Path) -> Result<Vec<(String, String, String)>> {
    let providers = LoadedConfig::load(config_path)?.value.providers;
    Ok(providers
        .into_iter()
        .map(|provider| (provider.id, provider.program, provider.url))
        .collect())
}

#[test]
fn registration_gives_a_program_its_own_port_once_and_keeps_its_credential() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
    let credential = root.join("credentials/runtime-t3.token");
    fs::write(&credential, "kept\n")?;
    let register = super::super::register;
    register(&config_path, Bundled::T3)?;
    let entry = |id: &str, program: &str, port: u16| {
        (
            id.to_owned(),
            program.to_owned(),
            format!("http://127.0.0.1:{port}"),
        )
    };
    let expected = [
        entry("discord-private", "discord", 44_001),
        entry("runtime-pi", "runtime-pi", 44_002),
        entry("runtime-t3", "runtime-t3", 44_003),
    ];
    assert_eq!(registered(&config_path)?, expected);
    assert_eq!(fs::read_to_string(&credential)?, "kept\n");
    let config = LoadedConfig::load(&config_path)?;
    let t3 = config.provider("runtime-t3", ProviderRole::Runtime)?;
    assert_eq!(t3.token_hash, hash_token("kept"));
    assert!(root.join("state/components/runtime-t3").is_dir());

    let written = fs::read(&config_path)?;
    for program in Bundled::ALL {
        register(&config_path, program)?;
    }
    assert_eq!(fs::read(&config_path)?, written);
    assert_eq!(fs::read_to_string(&credential)?, "kept\n");
    Ok(())
}

#[test]
fn registration_never_moves_a_provider_that_holds_its_port() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
    let moved = fs::read_to_string(&config_path)?.replace("127.0.0.1:44002", "127.0.0.1:44003");
    fs::write(&config_path, &moved)?;
    crate::assert_refused(
        super::super::register(&config_path, Bundled::T3),
        "runtime-t3 registers at http://127.0.0.1:44003, which runtime-pi has; give runtime-pi \
         another port in carillon.toml, then run this again",
    )?;
    assert_eq!(fs::read_to_string(&config_path)?, moved);
    assert!(!root.join("credentials/runtime-t3.token").exists());
    Ok(())
}
