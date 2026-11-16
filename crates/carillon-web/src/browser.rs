//! Who may use the page: a browser at an address the owner named, signed in
//! with the web chat's password.
//!
//! Any page the browser opens can send requests to loopback, and a
//! rebinding domain can resolve to it. A cookie is scoped by host and not by
//! port, so one set here would reach every other server on the machine.
//! The page therefore holds a bearer token in its origin's storage, which
//! no other origin can read and no browser attaches on its own.
//!
//! The root secret is the password's hash, a file only the installing user
//! reads. The token and every file link are HMACs of its text. The provider
//! reads the file for each request that presents one and keeps no key
//! between requests, so a new password voids them all at once, and the
//! command that sets it need not ask the provider.

use std::{
    fs::DirBuilder,
    os::unix::fs::DirBuilderExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use argon2::{
    Argon2,
    password_hash::{
        PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString, rand_core::OsRng,
    },
};
use axum::http::{
    HeaderMap, StatusCode,
    header::{HOST, ORIGIN},
};
use carillon_installation::{
    config::{LoadedConfig, component_directory},
    filesystem::atomic_write,
};
use carillon_interface::InterfaceError;
use hmac::{Hmac, Mac as _};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::Mutex;
use tracing::error;
use url::Url;

pub(crate) struct Browser {
    pub(crate) password_file: PathBuf,
    /// The provider's registry ID, which names its settings file.
    provider: String,
    pub(crate) listen: Address,
    /// The addresses besides `listen` the chat is opened at, from the
    /// provider's settings.
    addresses: Vec<Address>,
    /// Held across each guess at the password, and for a second more after
    /// a wrong one, so guesses are made one at a time.
    guessing: Arc<Mutex<()>>,
}

/// An address the chat is opened at, as a browser writes it in `Host` and
/// `Origin`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Address {
    pub(crate) host: String,
    pub(crate) origin: String,
}

impl Address {
    /// An address as `providers/<id>.toml` writes it: `https`, a host and an
    /// optional port. A browser gives a plain-http page of another machine
    /// no secure context, where the page cannot mint its IDs, and such a
    /// page would send the password unencrypted.
    pub(crate) fn parse(written: &str) -> Result<Self> {
        let url = Url::parse(written).with_context(|| format!("{written} is not an address"))?;
        let (Some(host), "https", "/", None, None, "", None) = (
            url.host_str(),
            url.scheme(),
            url.path(),
            url.query(),
            url.fragment(),
            url.username(),
            url.password(),
        ) else {
            bail!(
                "{written} is not an address a browser opens the chat at: write `https`, a host \
                 and an optional port, such as https://<machine>.<tailnet>.ts.net"
            );
        };
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        Ok(Self {
            host,
            origin: url.origin().ascii_serialization(),
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    addresses: Vec<String>,
}

/// The addresses `providers/<provider>.toml` names besides the one the
/// provider listens on. The file is optional.
pub(crate) fn addresses(loaded: &LoadedConfig, provider: &str) -> Result<Vec<Address>> {
    let path = loaded.provider_config_path(provider)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let settings: Settings =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    settings
        .addresses
        .iter()
        .map(|written| Address::parse(written).with_context(|| format!("in {}", path.display())))
        .collect()
}

/// Where provider `provider_id` keeps its password's hash.
pub(crate) fn password_file(data_dir: &Path, provider_id: &str) -> PathBuf {
    component_directory(data_dir, provider_id).join("password.hash")
}

/// Makes `password` the web chat's, which voids every token and file link
/// made before. The provider's directory is made when it has not started yet.
pub(crate) fn set_password(file: &Path, password: &str) -> Result<()> {
    let password = password.trim();
    if password.chars().count() < 12 {
        bail!("a password needs 12 characters or more, so the web chat's password is as it was");
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("hash the password: {error}"))?;
    if let Some(directory) = file.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .with_context(|| format!("create {}", directory.display()))?;
    }
    atomic_write(file, format!("{hash}\n").as_bytes(), 0o600)
        .with_context(|| format!("write {}", file.display()))
}

pub(crate) fn refusal(status: StatusCode, message: &str) -> InterfaceError {
    InterfaceError {
        status,
        message: message.to_owned(),
    }
}

/// Unix seconds.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The password's hash as one request read it.
#[derive(Clone)]
pub(crate) struct Key(String);

impl Key {
    /// The hash in `password_file`. A file that holds anything but a whole
    /// hash, its salt and its output included, holds none, and then nothing
    /// is signed in: a key without them could be guessed.
    pub(crate) fn read(password_file: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(password_file).ok()?;
        let key = text.trim();
        let hash = PasswordHash::new(key).ok()?;
        (hash.salt.is_some() && hash.hash.is_some()).then(|| Self(key.to_owned()))
    }

    /// The signature of `parts`. Each part is preceded by its length, so no
    /// two lists of parts sign alike, and the first says what is signed.
    pub(crate) fn sign(&self, parts: &[&str]) -> String {
        hex::encode(self.mac(parts).finalize().into_bytes())
    }

    pub(crate) fn matches(&self, parts: &[&str], signature: &str) -> bool {
        hex::decode(signature)
            .is_ok_and(|signature| self.mac(parts).verify_slice(&signature).is_ok())
    }

    fn mac(&self, parts: &[&str]) -> Hmac<Sha256> {
        let mut mac = match Hmac::<Sha256>::new_from_slice(self.0.as_bytes()) {
            Ok(mac) => mac,
            Err(error) => unreachable!("HMAC takes a key of any length: {error}"),
        };
        for part in parts {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part.as_bytes());
        }
        mac
    }

    /// The token a signed-in browser sends.
    pub(crate) fn token(&self) -> String {
        self.sign(&["token"])
    }

    pub(crate) fn authenticates(&self, token: &str) -> bool {
        self.matches(&["token"], token)
    }

    /// Whether this is the hash of `password`.
    fn admits(&self, password: &str) -> bool {
        PasswordHash::new(&self.0).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    }
}

impl Browser {
    pub(crate) fn new(
        data_dir: &Path,
        provider: String,
        authority: String,
        addresses: Vec<Address>,
    ) -> Self {
        Self {
            password_file: password_file(data_dir, &provider),
            provider,
            listen: Address {
                origin: format!("http://{authority}"),
                host: authority,
            },
            addresses,
            guessing: Arc::default(),
        }
    }

    /// What every request passes before anything reads it. `Host` must name
    /// one of the chat's addresses, so a rebinding domain is refused. An
    /// `Origin` must be one of them too; a request without one comes from
    /// the page's own navigation or from a program. A preflight carries its
    /// origin, so another origin's is refused here and it never sends the
    /// bearer.
    pub(crate) fn admit(&self, headers: &HeaderMap) -> Result<(), InterfaceError> {
        let addresses = || std::iter::once(&self.listen).chain(&self.addresses);
        let mut hosts = headers.get_all(HOST).iter();
        let host = hosts.next().filter(|_| hosts.next().is_none());
        if !host.is_some_and(|host| addresses().any(|address| address.host == *host)) {
            return Err(refusal(
                StatusCode::MISDIRECTED_REQUEST,
                &format!(
                    "This is Carillon's web chat at {}. To open it at another address, add that \
                     address to `addresses` in providers/{}.toml.",
                    self.listen.origin, self.provider
                ),
            ));
        }
        if headers
            .get_all(ORIGIN)
            .iter()
            .any(|origin| !addresses().any(|address| address.origin == *origin))
        {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "Carillon's web chat answers only its own page.",
            ));
        }
        Ok(())
    }

    /// The browser's token for `password`, when it is the web chat's. The
    /// guess runs in a task of its own, which keeps the turn to its end, so
    /// a request dropped midway frees no turn early.
    pub(crate) async fn sign_in(&self, password: String) -> Result<String, InterfaceError> {
        const WRONG: &str = "That is not the web chat's password.";
        let turn = Arc::clone(&self.guessing).lock_owned().await;
        let file = self.password_file.clone();
        let guess = tokio::spawn(async move {
            let _turn = turn;
            let Some(key) = Key::read(&file) else {
                return Err(refusal(
                    StatusCode::UNAUTHORIZED,
                    "The web chat has no password yet. Run `carillon web password` in a terminal \
                     to choose one.",
                ));
            };
            let checked =
                tokio::task::spawn_blocking(move || key.admits(password.trim()).then_some(key))
                    .await;
            match checked {
                Ok(Some(key)) => return Ok(key.token()),
                Ok(None) => {}
                Err(failed) => log_sign_in_task_failure(&failed),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            Err(refusal(StatusCode::UNAUTHORIZED, WRONG))
        });
        guess.await.unwrap_or_else(|failed| {
            log_sign_in_task_failure(&failed);
            Err(refusal(StatusCode::UNAUTHORIZED, WRONG))
        })
    }
}

/// The person sees a wrong password. A panic's message could quote what was
/// typed, so only the kind of failure is logged.
fn log_sign_in_task_failure(failed: &tokio::task::JoinError) {
    error!(
        panicked = failed.is_panic(),
        "a sign-in to the web chat ended before the password was checked"
    );
}
