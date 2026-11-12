//! Who may use the page: the installing user's own browser, signed in from
//! their terminal.
//!
//! Any page the browser opens can send requests to loopback, and a
//! rebinding domain can resolve to it. A cookie is scoped by host and not by
//! port, so one set here would reach every other server on the machine.
//! The page therefore holds a bearer token in its origin's storage, which
//! no other origin can read and no browser attaches on its own.
//!
//! The root secret is the browser key, a file only the installing user
//! reads. The token, a sign-in code and every file link are HMACs of it.
//! The provider reads the file for each request that presents one and keeps
//! no key between requests, so whoever replaces the file voids them all at
//! once, and the commands that sign in and out need not ask the provider.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{HOST, ORIGIN},
};
use carillon_installation::{
    config::{component_directory, new_token},
    filesystem::atomic_write,
};
use carillon_interface::InterfaceError;
use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use uuid::Uuid;

/// How many seconds `carillon web open`'s link works.
const CODE_LIFETIME: u64 = 2 * 60;

pub(crate) struct Browser {
    pub(crate) key_file: PathBuf,
    /// The `host:port` the provider listens on, as a browser writes it: the
    /// only name a request may give this provider.
    pub(crate) authority: String,
    /// The sign-in codes traded already, each by the name it carries, until
    /// it expires.
    pub(crate) used: Mutex<Vec<(String, u64)>>,
}

/// Where provider `provider_id` keeps its browser key.
pub(crate) fn key_file(data_dir: &Path, provider_id: &str) -> PathBuf {
    component_directory(data_dir, provider_id).join("browser.key")
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
        .map_or(0, |since| since.as_secs())
}

/// The browser key as one request read it.
#[derive(Clone)]
pub(crate) struct Key(String);

impl Key {
    /// The key in `key_file`. A file that is missing or empty holds none,
    /// and then nothing is signed in: an empty key would sign for anyone.
    pub(crate) fn read(key_file: &Path) -> Option<Self> {
        let key = std::fs::read_to_string(key_file).ok()?;
        Some(key.trim().to_owned())
            .filter(|key| !key.is_empty())
            .map(Self)
    }

    /// Writes a new key, which no token, code or link made with another
    /// matches, in the provider's directory, which its first start made.
    pub(crate) fn replace(key_file: &Path) -> io::Result<Self> {
        let key = new_token();
        atomic_write(key_file, key.as_bytes(), 0o600)?;
        Ok(Self(key))
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

    /// A code that signs one browser in, once, until `expires`: that time,
    /// a name of its own, and this key's signature of both.
    pub(crate) fn code(&self, expires: u64) -> String {
        let (expires, name) = (expires.to_string(), Uuid::now_v7().simple().to_string());
        let signature = self.sign(&["code", &expires, &name]);
        format!("{expires}.{name}.{signature}")
    }
}

/// The code of `carillon web open`'s link, made with the key in `key_file`,
/// which gets one when it has none.
pub(crate) fn new_code(key_file: &Path) -> io::Result<String> {
    let key = match Key::read(key_file) {
        Some(key) => key,
        None => Key::replace(key_file)?,
    };
    Ok(key.code(now() + CODE_LIFETIME))
}

impl Browser {
    pub(crate) fn origin(&self) -> String {
        format!("http://{}", self.authority)
    }

    /// What every request passes before anything reads it. `Host` must name
    /// this provider, so a rebinding domain is refused. An `Origin` must be
    /// this provider's own; a request without one comes from the page's own
    /// navigation or from a program. A preflight carries its origin, so
    /// another origin's is refused here and it never sends the bearer.
    pub(crate) fn admit(&self, headers: &HeaderMap) -> Result<(), InterfaceError> {
        let mut hosts = headers.get_all(HOST).iter();
        if hosts.next().map(HeaderValue::as_bytes) != Some(self.authority.as_bytes())
            || hosts.next().is_some()
        {
            return Err(refusal(
                StatusCode::MISDIRECTED_REQUEST,
                &format!("This is Carillon's web chat at {}.", self.origin()),
            ));
        }
        if headers
            .get_all(ORIGIN)
            .iter()
            .any(|origin| origin.as_bytes() != self.origin().as_bytes())
        {
            return Err(refusal(
                StatusCode::FORBIDDEN,
                "Carillon's web chat answers only its own page.",
            ));
        }
        Ok(())
    }

    /// The browser's token for `code`, when the key as it is now signed the
    /// code, it has not expired and no request traded it before.
    pub(crate) fn redeem(&self, code: &str) -> Option<String> {
        let key = Key::read(&self.key_file)?;
        let [expires, name, signature] = code.split('.').collect::<Vec<_>>()[..] else {
            return None;
        };
        let now = now();
        let until = expires.parse().ok().filter(|until: &u64| *until > now)?;
        if !key.matches(&["code", expires, name], signature) {
            return None;
        }
        // The signature covers the name letter for letter, so one code has
        // one name however the rest of it is respelled.
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        used.retain(|(_, until)| *until > now);
        if used.iter().any(|(spent, _)| spent == name) {
            return None;
        }
        used.push((name.to_owned(), until));
        Some(key.token())
    }
}
