//! What a runtime says a running turn is doing, asked over loopback.

use std::time::Duration;

use anyhow::anyhow;
use carillon_installation::config::{LoadedConfig, ProviderRole};
use carillon_protocol::model::{JOB_READ_LIMIT, TurnActivity};
use futures_util::{StreamExt as _, stream};
use reqwest::{Client, StatusCode, Url};
use tracing::debug;

use super::runtime_token;

/// How long one sample has, however many turns it asks about. A snapshot
/// is cosmetic: runtimes that are slow to describe their turns cost the
/// snapshots, not what reads them.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);

/// What each of `turns`, a runtime's ID and its turn's, is doing as its
/// runtime says, in their order: `None` where the runtime did not say
/// within [`SAMPLE_TIMEOUT`] of the sample's start. At most
/// [`JOB_READ_LIMIT`] are asked at once, so a turn behind that many
/// unanswered ones is not asked at all.
pub(super) async fn activities<'a>(
    client: &Client,
    config: &LoadedConfig,
    interface_id: &str,
    turns: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<Option<TurnActivity>> {
    let asked = turns
        .into_iter()
        .enumerate()
        .map(|(index, (runtime_id, turn_id))| async move {
            let said = runtime_activity(client, config, runtime_id, interface_id, turn_id).await;
            (index, said)
        })
        .collect::<Vec<_>>();
    let mut sample = vec![None; asked.len()];
    let mut answers = stream::iter(asked).buffer_unordered(JOB_READ_LIMIT);
    let answered = async {
        while let Some((index, said)) = answers.next().await {
            sample[index] = said;
        }
    };
    if tokio::time::timeout(SAMPLE_TIMEOUT, answered)
        .await
        .is_err()
    {
        debug!(
            interface_id,
            "runtimes did not describe every turn within {SAMPLE_TIMEOUT:?}"
        );
    }
    sample
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
        let response = client.get(url).bearer_auth(token).send().await?;
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
        debug!(
            runtime_id,
            turn_id,
            error = format!("{error:#}"),
            "runtime activity is unavailable"
        );
        None
    })
}
