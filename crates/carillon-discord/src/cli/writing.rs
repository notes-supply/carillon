//! Changes the CLI asks of the Discord settings file. The running provider
//! is the file's one writer: it applies the change and serves it. Only when
//! nothing answers at its address does the CLI change the file itself,
//! under the installation's maintenance lock, for the provider to serve
//! when it starts.

use anyhow::{Context as _, Result, bail};
use carillon_installation::config::LoadedConfig;

use super::{Applying, Operator, check_declared, release, report_applied};
use crate::config::{DiscordConfig, FileChange};

/// Applies `change` through the running provider, or to the stopped
/// provider's file, and says what became of it.
///
/// # Errors
///
/// Returns why the settings file is unchanged, or why a change written for
/// a restart would not start the provider.
pub(crate) async fn deliver(
    loaded: &LoadedConfig,
    provider: &str,
    change: FileChange,
) -> Result<()> {
    let operator = Operator::new(loaded, provider, "settings")?;
    match operator.change_settings(&change).await? {
        Applying::Applied(applied) => {
            report_applied(provider, &applied);
            if !applied.restart.is_empty() {
                written_for_restart(loaded, provider, &applied.restart).await?;
            }
        }
        Applying::Refused(refused) => bail!(
            "{refused}; the settings file is unchanged and {provider} keeps serving the settings \
             it has"
        ),
        Applying::Unreachable => {
            change.write(loaded, provider)?;
            println!(
                "The Discord provider does not answer at {}, so whether it serves these settings \
                 is not confirmed: the settings file was changed in place, and a stopped \
                 provider reads it when it starts.",
                operator.url
            );
        }
    }
    Ok(())
}

/// After the provider wrote a change to fields only a restart applies: the
/// saved file waits for `carillon restart` when this release accepts the
/// module it declares, which the provider did not try.
pub(super) async fn written_for_restart(
    loaded: &LoadedConfig,
    provider: &str,
    restart: &[String],
) -> Result<()> {
    let discord = DiscordConfig::load(loaded, provider)?;
    check_declared(loaded, provider, &discord, &release()?)
        .await
        .with_context(|| {
            format!("the settings file is saved, but `carillon restart` would not start {provider}")
        })?;
    println!(
        "{provider} keeps serving the settings it has: the saved file changes what only a \
         restart applies ({}). `carillon restart` serves it.",
        restart.join(", ")
    );
    Ok(())
}
