//! What a runtime says a running turn is doing, asked over loopback.

use std::time::Duration;

use anyhow::anyhow;
use carillon_installation::config::{LoadedConfig, ProviderRole};
use carillon_protocol::model::{JOB_READ_LIMIT, TurnActivity};
use futures_util::{StreamExt as _, stream};
use reqwest::{Client, StatusCode, Url};
use tracing::debug;

use super::runtime_token;

/// A snapshot is cosmetic: a runtime that is slow to describe a turn costs
/// the snapshot, not what reads it.
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

/// What each of `turns`, a runtime's ID and its turn's, is doing as its
/// runtime says, in their order: `None` where the runtime did not say.
/// At most [`JOB_READ_LIMIT`] are asked at once, so that many turns take
/// one snapshot timeout at most however many runtimes cannot answer.
pub(super) async fn activities<'a>(
    client: &Client,
    config: &LoadedConfig,
    interface_id: &str,
    turns: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<Option<TurnActivity>> {
    let asked = turns
        .into_iter()
        .map(|(runtime_id, turn_id)| {
            runtime_activity(client, config, runtime_id, interface_id, turn_id)
        })
        .collect::<Vec<_>>();
    stream::iter(asked).buffered(JOB_READ_LIMIT).collect().await
}

async fn runtime_activity(
    client: &Client,
    config: &LoadedConfig,
    runtime_id: &str,
    interface_id: &str,
    turn_id: &str,
) -> Option<TurnActivity> {
    let snapshot = async {
        let runtime = config.provider(runtime_id, ProviderRole::Runtime)?;
        let token = runtime_token(config, runtime)?;
        let mut url = Url::parse(&runtime.url)?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("runtime provider URL cannot have a path"))?
            .extend(["v1", "turns", turn_id, "activity"]);
        url.query_pairs_mut()
            .append_pair("interface_id", interface_id);
        let response = client
            .get(url)
            .bearer_auth(token)
            .timeout(ACTIVITY_TIMEOUT)
            .send()
            .await?;
        // Not yet or no longer executing in the runtime.
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok::<_, anyhow::Error>(Some(
            response.error_for_status()?.json::<TurnActivity>().await?,
        ))
    };
    // Interfaces ask every second or so; an unavailable snapshot is routine.
    snapshot.await.unwrap_or_else(|error| {
        debug!(runtime_id, turn_id, %error, "runtime activity is unavailable");
        None
    })
}
