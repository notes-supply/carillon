//! Everyday operation of an installed release: the owner's `carillon.target`
//! user service, its logs and health, and the lock that serializes
//! maintenance. `systemctl` and `tail` are used directly; Carillon does not
//! supervise processes itself.

use std::{
    fmt, fs,
    os::unix::{fs::OpenOptionsExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::Command as Process,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{LoadedConfig, validate_provider_identity};
use carillon_protocol::model::{Health, StatusReport};
use fs2::FileExt as _;
use reqwest::Url;
use serde::de::DeserializeOwned;

const TARGET: &str = "carillon.target";
/// The registered core unit records which installation the units run.
const CORE_UNIT: &str = "carillon-core.service";

/// Exclusive, non-blocking lock serializing maintenance of one installation.
/// The release scripts take the same `flock(2)` lock unless a caller already
/// holds it for them and says so through [`MaintenanceLock::HELD_FOR`].
pub(super) struct MaintenanceLock {
    _file: fs::File,
}

impl MaintenanceLock {
    pub(super) const HELD_FOR: &str = "CARILLON_MAINTENANCE_LOCK";

    pub(super) fn acquire(home: &Path) -> Result<Self> {
        let path = home.join(".maintenance.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("open maintenance lock {}", path.display()))?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "another Carillon maintenance operation is running for {}",
                home.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

/// Which installation, if any, the owner's Carillon user units belong to.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Registration {
    Absent,
    Owned,
    Other(PathBuf),
}

impl Registration {
    pub(super) fn of(home: &Path) -> Result<Self> {
        let unit = unit_directory()?.join(CORE_UNIT);
        let text = match fs::read_to_string(&unit) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(error) => return Err(error).with_context(|| format!("read {}", unit.display())),
        };
        let registered = registered_home(&text)
            .with_context(|| format!("{} does not name its installation", unit.display()))?;
        let same = match (home.canonicalize(), registered.canonicalize()) {
            (Ok(home), Ok(registered)) => home == registered,
            _ => home == registered,
        };
        Ok(if same {
            Self::Owned
        } else {
            Self::Other(registered)
        })
    }
}

/// Whether `carillon.target` was running when maintenance began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Activity {
    Running,
    Stopped,
}

impl fmt::Display for Activity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        })
    }
}

/// What the user service manager is asked to do with `carillon.target`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    const fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// The owner's user units, as they concern one installation.
pub(super) struct Services<'a> {
    home: &'a Path,
}

impl<'a> Services<'a> {
    /// # Errors
    /// Refuses when the units are absent or run another installation, so
    /// `--home` never starts or stops a different installation.
    pub(super) fn registered(home: &'a Path) -> Result<Self> {
        match Registration::of(home)? {
            Registration::Owned => Ok(Self { home }),
            Registration::Absent => bail!(
                "Carillon's user units are not registered; run {} to register {}",
                home.join("app/bin/carillon-register-services").display(),
                home.display()
            ),
            Registration::Other(other) => bail!(
                "Carillon's user units run the installation at {}, not {}; stop it and run {} to switch",
                other.display(),
                home.display(),
                home.join("app/bin/carillon-register-services").display()
            ),
        }
    }

    /// The units without checking which installation they run; for callers
    /// that have examined [`Registration`] themselves.
    pub(super) const fn new(home: &'a Path) -> Self {
        Self { home }
    }

    pub(super) fn activity(&self) -> Result<Activity> {
        let output = Process::new("systemctl")
            .args([
                "--user",
                "show",
                "--property=ActiveState",
                "--value",
                TARGET,
            ])
            .output()
            .context("run systemctl")?;
        if !output.status.success() {
            bail!(
                "cannot read {TARGET} state for {} from the user service manager: {}",
                self.home.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(match String::from_utf8_lossy(&output.stdout).trim() {
            "inactive" | "failed" => Activity::Stopped,
            _ => Activity::Running,
        })
    }

    /// Start, stop or restart the target; `PartOf=` propagates to services.
    pub(super) fn control(&self, action: Action) -> Result<()> {
        let verb = action.verb();
        let status = Process::new("systemctl")
            .args(["--user", verb, TARGET])
            .status()
            .context("run systemctl")?;
        if !status.success() {
            bail!(
                "systemctl --user {verb} {TARGET} failed ({status}) for {}",
                self.home.display()
            );
        }
        Ok(())
    }
}

fn unit_directory() -> Result<PathBuf> {
    let configuration = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        Some(directory) => PathBuf::from(directory),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    if !configuration.is_absolute() {
        bail!("HOME and XDG_CONFIG_HOME must be absolute paths");
    }
    Ok(configuration.join("systemd/user"))
}

/// Reverse the registration script's `Environment="CARILLON_HOME=…"` quoting.
fn registered_home(unit: &str) -> Option<PathBuf> {
    let quoted = unit
        .lines()
        .find_map(|line| line.strip_prefix("Environment=\"CARILLON_HOME="))?
        .strip_suffix('"')?;
    let mut value = String::with_capacity(quoted.len());
    let mut characters = quoted.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' | '%' => value.push(characters.next()?),
            character => value.push(character),
        }
    }
    Some(PathBuf::from(value))
}

/// Poll core until every enabled provider answers with its configured
/// identity. A version mismatch is reachable, not a failure; it is returned
/// for the caller to report.
pub(super) async fn wait_until_healthy(
    config: &LoadedConfig,
    timeout: Duration,
) -> Result<Vec<Health>> {
    let deadline = Instant::now() + timeout;
    loop {
        let failure = match status(config).await {
            Ok(report) => {
                let unavailable: Vec<_> = report
                    .providers
                    .iter()
                    .filter(|health| health.status == "unavailable")
                    .map(|health| health.id.as_str())
                    .collect();
                if unavailable.is_empty() {
                    return Ok(report.providers);
                }
                anyhow::anyhow!("unavailable: {}", unavailable.join(", "))
            }
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(failure.context(format!(
                "services were not healthy within {} seconds",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub(super) fn print_health(providers: &[Health]) {
    for health in providers {
        println!("  {}: {} ({})", health.id, health.status, health.version);
    }
    if providers
        .iter()
        .any(|health| health.status == "version_mismatch")
    {
        println!(
            "A version mismatch is diagnostic: update the provider's expected version in carillon.toml once the new release is intended."
        );
    }
}

/// Show the installation's service logs with `tail`, replacing this process.
pub(super) fn logs(home: &Path, service: Option<&str>, lines: u32, follow: bool) -> Result<()> {
    let directory = home.join("logs");
    let files = if let Some(service) = service {
        validate_provider_identity(service).context("invalid log name")?;
        vec![directory.join(format!("{service}.log"))]
    } else {
        let mut files: Vec<_> = fs::read_dir(&directory)
            .with_context(|| format!("read {}", directory.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
            .collect();
        files.sort();
        files
    };
    if files.is_empty() || (!follow && !files.iter().any(|file| file.exists())) {
        bail!("no logs yet in {}", directory.display());
    }
    let mut tail = Process::new("tail");
    tail.arg("-n").arg(lines.to_string());
    if follow {
        tail.arg("-F");
    }
    let error = tail.arg("--").args(&files).exec();
    Err(error).context("run tail")
}

pub(super) async fn status(config: &LoadedConfig) -> Result<StatusReport> {
    operator_get(config, operator_url(config, "/v1/status")?)
        .await
        .with_context(|| {
            format!(
                "core at {} did not answer; start Carillon with `carillon start`",
                config.value.core.listen
            )
        })
}

pub(super) fn operator_url(config: &LoadedConfig, path: &str) -> Result<Url> {
    Ok(Url::parse(&format!(
        "http://{}{path}",
        config.value.core.listen
    ))?)
}

pub(super) async fn operator_get<T: DeserializeOwned>(
    config: &LoadedConfig,
    url: Url,
) -> Result<T> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(config.credential("operator")?)
        .timeout(Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use anyhow::{Context, Result};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn registered_home_reverses_unit_quoting() {
        let unit = "[Service]\nEnvironment=\"HOME=/home/owner\"\n\
            Environment=\"CARILLON_HOME=/srv/100%% \\\"agent\\\" \\\\ root\"\n";
        assert_eq!(
            super::registered_home(unit),
            Some(PathBuf::from(r#"/srv/100% "agent" \ root"#))
        );
        assert_eq!(super::registered_home("[Service]\n"), None);
    }

    #[tokio::test]
    async fn operator_requests_time_out_on_stalled_headers_and_bodies() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config_path = crate::application::admin::initialize(
            &directory.path().join("installation"),
            directory.path(),
            44_000,
        )?;
        let config = super::LoadedConfig::load(&config_path)?;
        for partial_body in [false, true] {
            // Keep slow diagnostic fixtures away from backup's 127.0.0.1 probes.
            let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
            let url = format!("http://{}/v1/status", listener.local_addr()?).parse()?;
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await?);
                }
                if partial_body {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                        .await?;
                }
                // The request deadline must close this connection without a response.
                let mut remaining = Vec::new();
                stream.read_to_end(&mut remaining).await?;
                Ok::<_, std::io::Error>(())
            });
            let result = tokio::time::timeout(
                Duration::from_secs(7),
                super::operator_get::<serde_json::Value>(&config, url),
            )
            .await;
            server.abort();
            let error = result?.err().context("stalled diagnostics succeeded")?;
            assert!(
                error
                    .downcast_ref::<reqwest::Error>()
                    .context("not an HTTP error")?
                    .is_timeout()
            );
        }
        Ok(())
    }
}
