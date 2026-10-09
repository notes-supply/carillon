//! Everyday operation of an installed release: the owner's `carillon.target`
//! user service, its logs and health. `systemctl` and `tail` are used
//! directly; Carillon does not supervise processes itself. Maintenance is
//! serialized by `carillon_installation::maintenance::MaintenanceLock`, and
//! everything that reads or changes which installation the owner's units
//! run, by `UnitsLock`, held as [`HeldUnits`].

use std::{
    fmt, fs,
    os::unix::{fs::DirBuilderExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::Command as Process,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::{
    config::{LoadedConfig, validate_provider_identity},
    loopback,
    maintenance::UnitsLock,
};
use carillon_protocol::model::{Health, StatusReport};
use reqwest::Url;
use serde::{Deserialize, de::DeserializeOwned};

use super::http::{Applied, RegistryStatus};

const TARGET: &str = "carillon.target";
/// The registered core unit records which installation the units run.
const CORE_UNIT: &str = "carillon-core.service";

/// The owner's user units, which every installation of the owner shares,
/// locked against any other Carillon operation on them while held.
pub(super) struct HeldUnits {
    directory: PathBuf,
    _lock: UnitsLock,
}

impl HeldUnits {
    /// Lock the owner's unit directory, creating it as registration would.
    /// Take the installation's maintenance lock first.
    ///
    /// # Errors
    /// Refuses while another operation holds the units.
    pub(super) fn acquire() -> Result<Self> {
        let directory = unit_directory()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .with_context(|| format!("create {}", directory.display()))?;
        let directory = directory.canonicalize()?;
        let lock = UnitsLock::acquire(&directory)?;
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    /// The unit directory, as a child changing the units on this holder's
    /// behalf is told through [`UnitsLock::HELD_FOR`].
    pub(super) fn directory(&self) -> &Path {
        &self.directory
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
    /// Read while `units` are held, so the answer stays true until released.
    pub(super) fn of(home: &Path, units: &HeldUnits) -> Result<Self> {
        let unit = units.directory.join(CORE_UNIT);
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
    /// `--home` never starts or stops a different installation while
    /// `units` are held.
    pub(super) fn registered(home: &'a Path, units: &HeldUnits) -> Result<Self> {
        match Registration::of(home, units)? {
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
                "core at {} did not answer. If Carillon is stopped, `carillon start` starts it; \
                 otherwise `carillon logs core` shows why",
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
    loopback::client()
        .build()?
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

/// How the registry core serves stands beside the file, with its runtimes'
/// workers.
pub(super) async fn registry_status(config: &LoadedConfig) -> Result<RegistryStatus> {
    operator_get(config, operator_url(config, "/v1/registry")?)
        .await
        .with_context(|| {
            format!(
                "core at {} did not answer. If Carillon is stopped, it serves the file when it \
                 starts",
                config.value.core.listen
            )
        })
}

/// What core did with an operator's request to serve the registry file.
pub(super) enum Applying {
    /// It serves the file's live changes.
    Applied(Applied),
    /// It refused, saying why; what serves is unchanged.
    Refused(String),
    /// Nothing answers at the file's core address. That does not show core
    /// is stopped: a core started with another address still serves there,
    /// so whether it serves the file is not confirmed.
    Unreachable,
}

/// Asks the running core to serve the registry file.
///
/// # Errors
///
/// Returns an error when core may or may not have applied it.
pub(super) async fn apply_registry(config: &LoadedConfig) -> Result<Applying> {
    #[derive(Deserialize)]
    struct Refusal {
        error: String,
    }
    let unconfirmed = |error: reqwest::Error| {
        anyhow::anyhow!(
            "core did not confirm whether it applied the registry ({error}); `carillon registry` \
             shows what it serves"
        )
    };
    // An apply is a change: a request that may have reached core is never
    // sent again.
    let response = match loopback::client()
        .retry(reqwest::retry::never())
        .build()?
        .post(operator_url(config, "/v1/registry")?)
        .bearer_auth(config.credential("operator")?)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_connect() => return Ok(Applying::Unreachable),
        Err(error) => return Err(unconfirmed(error)),
    };
    let status = response.status();
    let body = response.bytes().await.map_err(unconfirmed)?;
    if status.is_success() {
        return Ok(Applying::Applied(serde_json::from_slice(&body).context(
            "core applied the registry but its answer could not be read; `carillon registry` \
             shows what it serves",
        )?));
    }
    let refusal = serde_json::from_slice::<Refusal>(&body).map_or_else(
        |_| format!("core refused the registry ({status})"),
        |refusal| refusal.error,
    );
    if status.is_client_error() {
        Ok(Applying::Refused(refusal))
    } else {
        bail!("{refusal}; `carillon registry` shows what core serves")
    }
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

    /// A core configured at `listen`, answering an apply the way `answer`
    /// says and counting the requests it receives.
    async fn applying_core(
        answer: Option<&'static [u8]>,
    ) -> Result<(
        super::LoadedConfig,
        tempfile::TempDir,
        tokio::task::JoinHandle<usize>,
    )> {
        let directory = tempfile::tempdir()?;
        let config_path = crate::application::admin::initialize(
            &directory.path().join("installation"),
            directory.path(),
            44_100,
        )?;
        let mut config = super::LoadedConfig::load(&config_path)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
        config.value.core.listen = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let mut requests = 0;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
            {
                requests += 1;
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let Ok(byte) = stream.read_u8().await else {
                        break;
                    };
                    request.push(byte);
                }
                // Without an answer the connection closes with none.
                if let Some(answer) = answer {
                    let _ = stream.write_all(answer).await;
                }
            }
            requests
        });
        Ok((config, directory, server))
    }

    #[tokio::test]
    async fn an_apply_is_sent_once_and_only_an_answer_confirms_it() -> Result<()> {
        // Nothing answering at the configured address does not confirm core
        // is stopped.
        let (mut config, _directory, server) = applying_core(None).await?;
        server.abort();
        let unused = tokio::net::TcpListener::bind("127.0.0.2:0").await?;
        config.value.core.listen = unused.local_addr()?;
        drop(unused);
        assert!(matches!(
            super::apply_registry(&config).await?,
            super::Applying::Unreachable
        ));
        // A refusal is core's answer, and leaves what serves in place.
        let refusal: &[u8] = b"HTTP/1.1 409 Conflict\r\nContent-Length: 31\r\n\r\n{\"error\":\"core.listen changed\"}";
        let (config, _directory, server) = applying_core(Some(refusal)).await?;
        let super::Applying::Refused(message) = super::apply_registry(&config).await? else {
            anyhow::bail!("a refusal was not reported as one");
        };
        assert_eq!(message, "core.listen changed");
        assert_eq!(server.await?, 1);
        // A request core received without answering may have been applied:
        // it is reported as unconfirmed and not sent again.
        let (config, _directory, server) = applying_core(None).await?;
        let unconfirmed = super::apply_registry(&config)
            .await
            .err()
            .context("an unanswered apply was confirmed")?;
        assert!(
            unconfirmed.to_string().contains("did not confirm"),
            "{unconfirmed}"
        );
        assert_eq!(server.await?, 1);
        Ok(())
    }
}
