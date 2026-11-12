//! The registry's own validation and the read-only check of an
//! installation, before any service starts.

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

#[test]
fn check_refuses_another_releases_database_unchanged_and_passes_an_installation_without_one()
-> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    assert_eq!(check(&state.config)?, CoreDatabase::Current);
    let data = &state.config.value.data_dir;
    let database = data.join("carillon.sqlite3");
    rusqlite::Connection::open(&database)?.execute_batch("DROP TABLE conversations")?;
    let before = fs::read(&database)?;
    let aside = format!(
        "mv '{}'* '{}",
        database.display(),
        directory.path().display()
    );
    crate::assert_refused(check(&state.config), &aside)?;
    assert_eq!(fs::read(&database)?, before);
    fs::rename(&database, directory.path().join("aside"))?;
    assert_eq!(check(&state.config)?, CoreDatabase::Absent);
    assert!(!database.exists());
    Ok(())
}

#[test]
fn check_passes_no_database_only_where_nothing_is() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let database = state.config.value.data_dir.join("carillon.sqlite3");
    fs::remove_file(&database)?;
    std::os::unix::fs::symlink(directory.path().join("nowhere"), &database)?;
    crate::assert_refused(check(&state.config), "unable to open database file")
}
