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
    state.config.value.core.listen.set_port(0);
    fs::write(&path, toml::to_string(&state.config.value)?)?;
    crate::assert_refused(
        LoadedConfig::load(&path),
        "core listen port must be nonzero",
    )?;
    Ok(())
}
