//! On-demand measurements of the Discord module's cards and live
//! replacement, for release builds of the built module:
//!
//! ```sh
//! mise run build:module
//! cargo test --release -p carillon-discord --lib module::measurement -- --ignored --nocapture
//! ```
//!
//! `CARILLON_CARD_INPUTS` may name a JSON array of `{"name", "request"}`
//! card requests to measure instead of the recorded parity cases. Each
//! measurement prints JSON lines; none asserts a duration.

use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context as _;
use carillon_discord_module::{
    HostCalls,
    contract::{
        Card, CardRequest, HostReply, HostRequest, Interaction, InteractionKind, Invocation,
    },
};
use carillon_protocol::model::ModelQuery;
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;

use super::{
    Calls, Host, InteractionCalls, Module,
    tests::{files, release_module_bytes},
};

/// The requests to draw: the recorded parity cases, or `CARILLON_CARD_INPUTS`.
fn requests() -> anyhow::Result<Vec<(String, CardRequest)>> {
    #[derive(serde::Deserialize)]
    struct Case {
        name: String,
        request: CardRequest,
    }
    let text = match std::env::var_os("CARILLON_CARD_INPUTS") {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.to_string_lossy()))?,
        None => include_str!("../../../carillon-discord-module/fixtures/cards.json").to_owned(),
    };
    Ok(serde_json::from_str::<Vec<Case>>(&text)?
        .into_iter()
        .map(|case| (case.name, case.request))
        .collect())
}

/// The guest's own code, compiled into this program, makes no native call.
struct Refusing;

impl HostCalls for Refusing {
    fn call(&mut self, _: HostRequest) -> Result<HostReply, String> {
        Err("no native calls".to_owned())
    }
}

/// The card the guest's code draws natively, as the host takes it.
fn native(request: CardRequest) -> anyhow::Result<Option<Embed>> {
    match carillon_discord_module::handle(&mut Refusing, Invocation::Card(Box::new(request))) {
        carillon_discord_module::contract::Outcome::Card(Card::Show(embed)) => {
            let embed: Embed = serde_json::from_value(embed)?;
            twilight_validate::embed::embed(&embed)?;
            Ok(Some(embed))
        }
        carillon_discord_module::contract::Outcome::Card(Card::Remove) => Ok(None),
        _ => anyhow::bail!("no card"),
    }
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// The `per_mille` percentile of `samples`, nearest rank.
fn percentile(samples: &mut [Duration], per_mille: usize) -> f64 {
    samples.sort_unstable();
    let rank = (samples.len() * per_mille).div_ceil(1000);
    micros(samples[rank.clamp(1, samples.len()) - 1])
}

fn summary(samples: &mut [Duration]) -> Value {
    json!({
        "n": samples.len(),
        "p50_us": percentile(samples, 500),
        "p99_us": percentile(samples, 990),
        "max_us": percentile(samples, 1000),
    })
}

/// This process's resident and peak resident memory, in KiB.
fn memory() -> Value {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim().trim_end_matches(" kB").parse::<u64>().ok())
    };
    json!({"rss_kib": field("VmRSS:"), "hwm_kib": field("VmHWM:")})
}

async fn release(directory: &std::path::Path) -> anyhow::Result<Module> {
    std::fs::write(directory.join("release.wasm"), release_module_bytes()?)?;
    Module::load(files(directory)).await
}

const SAMPLES: usize = 300;
const WARMUP: usize = 20;
/// Jobs the monitor reads at most per page of jobs at work.
const BATCH: usize = 32;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run in a release build with the built module"]
async fn card_rendering_cost() -> anyhow::Result<()> {
    let bytes = release_module_bytes()?;
    let host = Host::<Calls>::new()?;
    let started = Instant::now();
    host.load(&bytes)?;
    let compile = started.elapsed();
    let directory = tempfile::tempdir()?;
    let started = Instant::now();
    let module = Arc::new(release(directory.path()).await?);
    let load = started.elapsed();
    println!(
        "{}",
        json!({
            "measure": "cold",
            "module_bytes": bytes.len(),
            "compile_us": micros(compile),
            "load_us": micros(load),
            "memory": memory(),
        })
    );
    let requests = requests()?;
    for (name, request) in &requests {
        let size = serde_json::to_vec(request)?.len();
        let expected = native(request.clone())?;
        assert_eq!(module.render_card(request).await?, expected, "{name}");
        let mut copies = vec![request.clone(); SAMPLES + WARMUP];
        let mut native_samples = Vec::with_capacity(SAMPLES);
        for (index, copy) in copies.drain(..).enumerate() {
            let started = Instant::now();
            native(copy)?;
            if index >= WARMUP {
                native_samples.push(started.elapsed());
            }
        }
        let mut wasm_samples = Vec::with_capacity(SAMPLES);
        for index in 0..SAMPLES + WARMUP {
            let started = Instant::now();
            module.render_card(request).await?;
            if index >= WARMUP {
                wasm_samples.push(started.elapsed());
            }
        }
        println!(
            "{}",
            json!({
                "measure": "card",
                "case": name,
                "request_bytes": size,
                "native": summary(&mut native_samples),
                "wasm": summary(&mut wasm_samples),
            })
        );
    }
    for (name, request) in &requests {
        let mut sequential = Vec::new();
        let mut concurrent = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            for _ in 0..BATCH {
                module.render_card(request).await?;
            }
            sequential.push(started.elapsed());
            let started = Instant::now();
            let mut batch = tokio::task::JoinSet::new();
            for _ in 0..BATCH {
                let (module, request) = (module.clone(), request.clone());
                batch.spawn(async move { module.render_card(&request).await.map(|_| ()) });
            }
            while let Some(drawn) = batch.join_next().await {
                drawn??;
            }
            concurrent.push(started.elapsed());
        }
        println!(
            "{}",
            json!({
                "measure": "batch",
                "case": name,
                "jobs": BATCH,
                "read_interval_us": micros(crate::activity::POLL_INTERVAL),
                "sequential": summary(&mut sequential),
                "concurrent": summary(&mut concurrent),
                "memory": memory(),
            })
        );
    }
    Ok(())
}

/// The release module with a custom section added: another module to the
/// provider, drawing the same cards.
fn variant(bytes: &[u8]) -> Vec<u8> {
    let name = b"carillon-measurement";
    let mut variant = bytes.to_vec();
    variant.push(0);
    variant.push(u8::try_from(name.len() + 1).unwrap_or(u8::MAX));
    variant.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
    variant.extend_from_slice(name);
    variant
}

/// What the load beside the replacements saw.
#[derive(Default)]
struct Seen {
    drawn: AtomicUsize,
    wrong: AtomicUsize,
    failed: AtomicUsize,
    answered: AtomicUsize,
    unanswered: AtomicUsize,
}

/// Requests, each with the card the guest's code draws for it natively.
type Cases = Arc<[(CardRequest, Option<Embed>)]>;

/// Draws the cases' cards one after another until `stop`, each expected to
/// be what the guest's code draws natively.
async fn draw(
    module: Arc<Module>,
    cases: Cases,
    start: usize,
    stop: Arc<AtomicBool>,
    seen: Arc<Seen>,
) {
    let mut index = start;
    while !stop.load(Ordering::Relaxed) {
        let (request, expected) = &cases[index % cases.len()];
        let counter = match module.render_card(request).await {
            Ok(card) if card == *expected => &seen.drawn,
            Ok(_) => &seen.wrong,
            Err(_) => &seen.failed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        index += 1;
    }
}

/// Runs `/model` until `stop`. Core is not there, so the module answers
/// with its native call's refusal.
async fn interact(module: Arc<Module>, stop: Arc<AtomicBool>, seen: Arc<Seen>) {
    while !stop.load(Ordering::Relaxed) {
        let calls = crate::client::test_client("http://127.0.0.1:1".to_owned()).map(|core| {
            InteractionCalls::new(
                core,
                ModelQuery {
                    agent_id: "default".to_owned(),
                    audience_id: None,
                },
            )
        });
        let interaction = Interaction {
            kind: InteractionKind::Command,
            name: "model".to_owned(),
            values: Vec::new(),
            options: Value::Null,
            conversation: false,
        };
        let replied = match calls {
            Ok(calls) => module.invoke(calls, interaction).await.is_ok(),
            Err(_) => false,
        };
        let counter = if replied {
            &seen.answered
        } else {
            &seen.unanswered
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// One round of replacements under load: the generations it activated and
/// how long each change took.
async fn round(
    module: &Arc<Module>,
    cases: &Cases,
    modules: (&[u8], usize),
    seen: &Arc<Seen>,
) -> anyhow::Result<(Vec<Weak<super::Generation>>, Vec<Duration>)> {
    const RENDERERS: usize = 32;
    const INTERACTIONS: usize = 10;
    let (other, installs) = modules;
    let stop = Arc::new(AtomicBool::new(false));
    let mut load = tokio::task::JoinSet::new();
    for start in 0..RENDERERS {
        let draw = draw(
            module.clone(),
            cases.clone(),
            start,
            stop.clone(),
            seen.clone(),
        );
        load.spawn(draw);
    }
    for _ in 0..INTERACTIONS {
        load.spawn(interact(module.clone(), stop.clone(), seen.clone()));
    }
    let (mut generations, mut changes) = (Vec::new(), Vec::new());
    for install in 0..installs {
        let started = Instant::now();
        if install % 2 == 0 {
            module.install(other, None).await?;
        } else {
            module.reset(None).await?;
        }
        changes.push(started.elapsed());
        generations.push(Arc::downgrade(&module.current()));
    }
    stop.store(true, Ordering::Relaxed);
    while let Some(task) = load.join_next().await {
        task?;
    }
    Ok((generations, changes))
}

/// Rounds of 20 replacements, each under 32 card drawers and 10
/// interactions, in one process: memory after each round's load has ended
/// shows whether it settles. `CARILLON_MEASURE_ROUNDS` sets how many.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run in a release build with the built module"]
async fn replacement_under_load() -> anyhow::Result<()> {
    const INSTALLS: usize = 20;
    let rounds = std::env::var("CARILLON_MEASURE_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(1);
    let bytes = release_module_bytes()?;
    let other = variant(&bytes);
    let directory = tempfile::tempdir()?;
    let module = Arc::new(release(directory.path()).await?);
    let before = memory();
    let cases: Cases = requests()?
        .into_iter()
        .map(|(_, request)| Ok((request.clone(), native(request)?)))
        .collect::<anyhow::Result<_>>()?;
    let seen = Arc::new(Seen::default());
    let (mut changes, mut after) = (Vec::new(), Vec::new());
    let mut retained = 0;
    for _ in 0..rounds {
        let (generations, round_changes) =
            round(&module, &cases, (&other, INSTALLS), &seen).await?;
        changes.extend(round_changes);
        // Every generation but the serving one is gone once its work is.
        let serving = module.current();
        retained += generations
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|generation| !Arc::ptr_eq(generation, &serving))
            .count();
        after.push(memory());
    }
    let count = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    println!(
        "{}",
        json!({
            "measure": "replacement",
            "rounds": rounds,
            "installs_per_round": INSTALLS,
            "cards_drawn": count(&seen.drawn),
            "cards_wrong": count(&seen.wrong),
            "cards_failed": count(&seen.failed),
            "interactions_answered": count(&seen.answered),
            "interactions_unanswered": count(&seen.unanswered),
            "change": summary(&mut changes),
            "earlier_generations_retained": retained,
            "memory_before": before,
            "memory_after_each_round": after,
        })
    );
    assert_eq!(count(&seen.wrong) + count(&seen.failed), 0);
    assert_eq!(count(&seen.unanswered), 0);
    assert_eq!(retained, 0, "an earlier generation outlived its work");
    Ok(())
}
