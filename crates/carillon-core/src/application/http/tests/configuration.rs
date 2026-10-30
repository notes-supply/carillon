//! The registry's own validation, before any service starts.

use std::fs;

use tempfile::tempdir;

use super::installation;
use crate::application::http::*;

#[test]
fn configuration_rejects_malformed_provider_endpoints_before_startup() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let path = directory.path().join("carillon.toml");
    for index in 0..state.config.value.providers.len() {
        let original = state.config.value.providers[index].url.clone();
        for url in [
            "not-a-url",
            "",
            "https://127.0.0.1:3000",
            "http://localhost:3000",
            "http://127.0.0.1",
            "http://127.0.0.1:99999",
            "http://127.0.0.1:3000/path",
            "http://user@127.0.0.1:3000",
            "http://127.0.0.1:3000?query",
            "http://0.0.0.0:3000",
            "http://192.168.1.2:3000",
            "http://[::]:3000",
            "http://[2001:db8::1]:3000",
            "http://127.0.0.1:0",
            "http://[::1]:0",
        ] {
            state.config.value.providers[index].url = url.to_owned();
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            crate::assert_refused(LoadedConfig::load(&path), "invalid URL for provider")?;
        }
        for url in [
            "http://127.0.0.1:3000",
            "http://127.2.3.4:3000",
            "http://[::1]:3000",
        ] {
            state.config.value.providers[index].url = url.to_owned();
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            LoadedConfig::load(&path)?;
        }
        state.config.value.providers[index].url = original;
    }
    // Concurrent conversations are a bounded runtime setting.
    for index in 0..state.config.value.providers.len() {
        let runtime = state.config.value.providers[index].role == ProviderRole::Runtime;
        for (parallel, valid) in [(0, false), (1, runtime), (16, runtime), (17, false)] {
            state.config.value.providers[index].parallel_turns = Some(parallel);
            fs::write(&path, toml::to_string(&state.config.value)?)?;
            let id = &state.config.value.providers[index].id;
            assert_eq!(
                LoadedConfig::load(&path)
                    .err()
                    .map(|error| error.to_string()),
                (!valid).then(|| format!(
                    "provider {id} parallel_turns must be between 1 and 16, on a runtime"
                )),
                "parallel_turns {parallel}"
            );
        }
        state.config.value.providers[index].parallel_turns = None;
    }
    state.config.value.core.listen.set_port(0);
    fs::write(&path, toml::to_string(&state.config.value)?)?;
    crate::assert_refused(
        LoadedConfig::load(&path),
        "core listen port must be nonzero",
    )?;
    Ok(())
}

/// A durable runtime holds and orders its own turns: only a runtime is one,
/// never with `parallel_turns`, and only a restart makes one durable or not.
#[test]
fn only_a_runtime_without_parallel_turns_is_durable_and_only_at_a_restart() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    let path = directory.path().join("carillon.toml");
    let serving = state.config.value.clone();
    fs::write(&path, toml::to_string(&serving)?)?;
    assert!(!fs::read_to_string(&path)?.contains("durable"));
    for (index, parallel_turns, valid) in [(1, None, true), (1, Some(1), false), (0, None, false)] {
        let mut file = serving.clone();
        file.providers[index].durable = true;
        file.providers[index].parallel_turns = parallel_turns;
        fs::write(&path, toml::to_string(&file)?)?;
        let id = &file.providers[index].id;
        assert_eq!(
            LoadedConfig::load(&path).err().map(|error| error.to_string()),
            (!valid).then(|| format!(
                "provider {id} durable is for a runtime, which then orders its own turns without parallel_turns"
            )),
            "{id} durable with parallel_turns {parallel_turns:?}"
        );
    }
    state.config.value.providers[1].durable = true;
    let difference = registry::Difference::between(&serving, &state.config.value);
    assert_eq!(
        (difference.live, difference.restart),
        (
            Vec::new(),
            vec!["providers.test-runtime.durable".to_owned()]
        )
    );
    Ok(())
}
