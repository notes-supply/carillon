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
//! reads. The token and every file link are HMACs of it, so replacing the
//! key voids them all and the key itself never leaves this process.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::{Context as _, Result};
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
use tokio::time::Instant;

/// How long `carillon web open`'s link works.
const CODE_LIFETIME: Duration = Duration::from_mins(2);

pub(crate) struct Browser {
    key_file: PathBuf,
    /// The `host:port` of the provider's registry URL, the only name a
    /// request may give this provider.
    authority: String,
    state: Mutex<State>,
}

struct State {
    key: String,
    /// Sign-in codes not yet used, each with when it stops working.
    codes: Vec<(String, Instant)>,
}

/// Where provider `provider_id` keeps its browser key.
pub(crate) fn key_file(data_dir: &Path, provider_id: &str) -> PathBuf {
    component_directory(data_dir, provider_id).join("browser.key")
}

/// Writes a new browser key, which no token or link made with another
/// matches.
pub(crate) fn replace_key(key_file: &Path) -> io::Result<String> {
    let key = new_token();
    atomic_write(key_file, key.as_bytes(), 0o600)?;
    Ok(key)
}

pub(crate) fn refusal(status: StatusCode, message: &str) -> InterfaceError {
    InterfaceError {
        status,
        message: message.to_owned(),
    }
}

impl Browser {
    /// Reads the key, creating it when there is none, in a directory that
    /// exists.
    pub(crate) fn open(key_file: PathBuf, authority: String) -> Result<Self> {
        let key = match std::fs::read_to_string(&key_file) {
            Ok(key) if !key.trim().is_empty() => key.trim().to_owned(),
            Ok(_) => replace_key(&key_file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => replace_key(&key_file)?,
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", key_file.display()));
            }
        };
        Ok(Self {
            key_file,
            authority,
            state: Mutex::new(State {
                key,
                codes: Vec::new(),
            }),
        })
    }

    pub(crate) fn origin(&self) -> String {
        format!("http://{}", self.authority)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// A code `carillon web open` puts in its link: it signs one browser
    /// in, once, within [`CODE_LIFETIME`].
    pub(crate) fn issue_code(&self) -> String {
        let code = new_token();
        let now = Instant::now();
        let mut state = self.state();
        state.codes.retain(|(_, until)| *until > now);
        state.codes.push((code.clone(), now + CODE_LIFETIME));
        code
    }

    /// The browser's token for a code still good, which the code then no
    /// longer is.
    pub(crate) fn redeem(&self, code: &str) -> Option<String> {
        let now = Instant::now();
        let mut state = self.state();
        state.codes.retain(|(_, until)| *until > now);
        let index = state.codes.iter().position(|(issued, _)| issued == code)?;
        state.codes.swap_remove(index);
        Some(hex::encode(
            mac(&state.key, &["token"]).finalize().into_bytes(),
        ))
    }

    /// Whether `token` is the one the current key gives.
    pub(crate) fn authenticates(&self, token: &str) -> bool {
        self.matches(&["token"], token)
    }

    /// Replaces the key: every browser is signed out, every file link is
    /// void and no code waits.
    pub(crate) fn sign_out(&self) -> Result<()> {
        // Held across the write, so no request is answered with the old key
        // once this returns.
        let mut state = self.state();
        state.key = replace_key(&self.key_file)
            .with_context(|| format!("replace {}", self.key_file.display()))?;
        state.codes.clear();
        Ok(())
    }

    /// The signature that lets a link read what `parts` name.
    pub(crate) fn sign(&self, parts: &[&str]) -> String {
        hex::encode(mac(&self.state().key, parts).finalize().into_bytes())
    }

    /// Whether `signature` is the current key's for `parts`.
    pub(crate) fn matches(&self, parts: &[&str], signature: &str) -> bool {
        hex::decode(signature).is_ok_and(|signature| {
            mac(&self.state().key, parts)
                .verify_slice(&signature)
                .is_ok()
        })
    }
}

/// Each part is preceded by its length, so no two lists of parts sign alike.
fn mac(key: &str, parts: &[&str]) -> Hmac<Sha256> {
    let mut mac = match Hmac::<Sha256>::new_from_slice(key.as_bytes()) {
        Ok(mac) => mac,
        Err(error) => unreachable!("HMAC takes a key of any length: {error}"),
    };
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part.as_bytes());
    }
    mac
}
