use crate::CoreClient;
use anyhow::{Context as _, Result, bail};
use carillon_installation::config::{
    LoadedConfig, Provider, ProviderRole, RELEASE_VERSION, read_token_file, token_matches,
};
use carillon_protocol::model::Health;
use std::path::Path;
use tokio::net::TcpListener;
use tracing::info;

/// An interface provider that may serve.
pub struct Started {
    /// Its registry entry.
    pub provider: Provider,
    /// Bound to the entry's address, which makes this process the entry's
    /// only instance. `carillon start` waits for `/health` on it.
    pub listener: TcpListener,
    pub core: CoreClient,
    /// What `GET /health` answers: core checks the ID and role against the
    /// registry.
    pub health: Health,
}

/// Starts interface provider `provider_id` of `config`: `None` when the
/// registry does not have it or has it disabled, which ends the program
/// successfully, so its service is not restarted, before it reads its
/// credential or touches any state. Otherwise its credential is checked
/// against the registry and its address bound, in that order.
///
/// # Errors
///
/// Returns an error when the entry is not an interface, the credential does
/// not match it, or its address cannot be bound.
pub async fn start(
    config: &LoadedConfig,
    provider_id: &str,
    token_file: &Path,
) -> Result<Option<Started>> {
    let providers = &config.value.providers;
    if !providers.iter().any(|provider| provider.id == provider_id) {
        info!("interface provider {provider_id} is not in carillon.toml; not serving");
        return Ok(None);
    }
    let provider = config.provider(provider_id, ProviderRole::Interface)?;
    if !provider.enabled {
        info!("interface provider {provider_id} is disabled; not serving");
        return Ok(None);
    }
    let token = read_token_file(token_file)?;
    if !token_matches(&provider.token_hash, &token) {
        bail!("interface provider {provider_id} token does not match configuration");
    }
    let address = provider.listen_address()?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind interface provider {provider_id} to {address}"))?;
    Ok(Some(Started {
        health: Health {
            id: provider.id.clone(),
            role: provider.role.as_str().to_owned(),
            version: RELEASE_VERSION.to_owned(),
            status: "ready".to_owned(),
        },
        core: CoreClient::new(
            format!("http://{}", config.value.core.listen),
            provider.id.clone(),
            token,
        )?,
        provider: provider.clone(),
        listener,
    }))
}
