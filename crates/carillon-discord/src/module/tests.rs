use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use axum::{Router, http::StatusCode, routing::put};
use carillon_discord_module::contract::{CardRequest, Interaction, InteractionKind};
use carillon_installation::maintenance::MaintenanceLock;
use serde_json::{Value, json};

use super::{
    Change, ChangeError, Declared, DiscordApp, MODULE_LIMIT, Module, ModuleFiles, Registration,
    Source,
};

/// `text` as the contents of a WAT string.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A module is read from a regular file or a link to one. A directory,
/// a device or a FIFO is refused without waiting on it.
#[test]
fn only_a_regular_module_file_is_read() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("module.wasm");
    std::fs::write(&file, b"module")?;
    let link = directory.path().join("link.wasm");
    std::os::unix::fs::symlink(&file, &link)?;
    assert_eq!(super::read_module(&file)?, b"module");
    assert_eq!(super::read_module(&link)?, b"module");
    let fifo = directory.path().join("fifo.wasm");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    for path in [
        fifo,
        directory.path().to_owned(),
        PathBuf::from("/dev/null"),
    ] {
        // Bounded from outside: a read that waited would fail the test,
        // not hold up the gate.
        let (sent, received) = std::sync::mpsc::channel();
        let read = path.clone();
        std::thread::spawn(move || {
            sent.send(super::read_module(&read).map_err(|error| error.kind()))
        });
        let outcome = received
            .recv_timeout(Duration::from_secs(5))
            .with_context(|| format!("reading {} did not return", path.display()))?;
        assert_eq!(
            outcome,
            Err(std::io::ErrorKind::InvalidInput),
            "{}",
            path.display()
        );
    }
    Ok(())
}

/// A module that answers `Describe` with `description`, a card with an
/// embed that says so, and every other invocation with `reply`, in binary
/// form.
pub(crate) fn module(description: &str, reply: &str) -> anyhow::Result<Vec<u8>> {
    module_with_card(description, reply, &showing("A test card"))
}

/// [`module`], answering every card with `card`.
pub(crate) fn module_with_card(
    description: &str,
    reply: &str,
    card: &str,
) -> anyhow::Result<Vec<u8>> {
    let describe_length =
        serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
    // A card request begins `{"Card"`; an interaction `{"Interaction"`.
    let text = format!(
        r#"(module
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 16384) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 32768))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        (if (result i32) (i32.eq (i32.load8_u (i32.const 32770)) (i32.const 67))
          (then (global.set $output (i32.const 16384)) (i32.const {}))
          (else (global.set $output (i32.const 4096)) (i32.const {}))))))
  (func (export "output") (result i32) (global.get $output)))"#,
        escape(description),
        escape(reply),
        escape(card),
        description.len(),
        card.len(),
        reply.len(),
    );
    Ok(wat::parse_str(text)?)
}

/// The release module as built for this tree: the file
/// `CARILLON_DISCORD_MODULE` names, or `mise run build:module`'s output.
pub(crate) fn release_module_bytes() -> anyhow::Result<Vec<u8>> {
    let path = std::env::var_os("CARILLON_DISCORD_MODULE").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/wasm32-unknown-unknown/wasm/carillon_discord_bundled.wasm")
        },
        PathBuf::from,
    );
    std::fs::read(&path).with_context(|| {
        format!(
            "read the built Discord module {}; build it with `mise run build:module`",
            path.display()
        )
    })
}

/// The example module as built for this tree: the file
/// `CARILLON_EXAMPLE_MODULE` names, or `mise run build:module`'s output.
pub(crate) fn example_module_bytes() -> anyhow::Result<Vec<u8>> {
    let path = std::env::var_os("CARILLON_EXAMPLE_MODULE").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/wasm32-unknown-unknown/wasm/carillon_card_colours.wasm")
        },
        PathBuf::from,
    );
    std::fs::read(&path).with_context(|| {
        format!(
            "read the built example module {}; build it with `mise run build:module`",
            path.display()
        )
    })
}

/// The release module, compiled once for the tests that draw real cards.
pub(crate) async fn release_module() -> anyhow::Result<Arc<Module>> {
    static RELEASE: tokio::sync::OnceCell<Arc<Module>> = tokio::sync::OnceCell::const_new();
    RELEASE
        .get_or_try_init(|| async {
            let directory = tempfile::tempdir()?;
            std::fs::write(
                directory.path().join("release.wasm"),
                release_module_bytes()?,
            )?;
            Ok::<_, anyhow::Error>(Arc::new(
                Module::load(files(directory.path()), Source::Bundled).await?,
            ))
        })
        .await
        .cloned()
}

/// A card answer showing an embed described by `text`.
pub(crate) fn showing(text: &str) -> String {
    json!({"Card": {"Show": {"type": "rich", "description": text}}}).to_string()
}

/// [`module`], written to `path`.
pub(crate) fn write_module(path: &Path, description: &str, reply: &str) -> anyhow::Result<()> {
    std::fs::write(path, module(description, reply)?)?;
    Ok(())
}

pub(crate) fn describing(commands: &[Value]) -> String {
    json!({"Description": {"contract": carillon_discord_module::contract::CONTRACT, "commands": commands}}).to_string()
}

pub(crate) fn replying(content: &str, components: &[Value]) -> String {
    json!({"Reply": {"content": content, "components": components}}).to_string()
}

pub(crate) fn definition(name: &str) -> Value {
    json!({"type": 1, "name": name, "description": "A test command"})
}

/// Command files in `root`: the bundled module at `release.wasm`, the
/// record under `state/`, and `root` as the installation.
pub(crate) fn files(root: &Path) -> ModuleFiles {
    ModuleFiles {
        bundled: root.join("release.wasm"),
        record: root.join("state/commands.json"),
        installation: root.to_path_buf(),
    }
}

/// The custom module `name` in `root`.
pub(crate) fn custom(root: &Path, name: &str) -> Source {
    Source::Custom {
        path: root.join(name),
    }
}

/// What an apply that changed no module reports, for tests of settings
/// alone.
pub(crate) fn unchanged() -> Change {
    let summary = super::ModuleSummary {
        source: Source::Bundled,
        commands: Vec::new(),
        digest: String::new(),
    };
    Change {
        serving: summary.clone(),
        previous: summary,
        discord: Registration::NotConnected,
    }
}

pub(crate) async fn release_commands(
    root: &Path,
    names: &[&str],
    reply: &str,
) -> anyhow::Result<Module> {
    let definitions: Vec<Value> = names.iter().map(|name| definition(name)).collect();
    write_module(
        &root.join("release.wasm"),
        &describing(&definitions),
        &replying(reply, &[]),
    )?;
    Module::load(files(root), Source::Bundled).await
}

pub(crate) fn variant(names: &[&str], reply: &str) -> anyhow::Result<Vec<u8>> {
    let definitions: Vec<Value> = names.iter().map(|name| definition(name)).collect();
    module(&describing(&definitions), &replying(reply, &[]))
}

/// The application and guild a provider would act for, talking to the
/// Discord API at `address`.
pub(crate) fn app(address: &str, application_id: u64, guild_id: u64) -> DiscordApp {
    DiscordApp {
        http: Arc::new(
            twilight_http::Client::builder()
                .token("test-only".to_owned())
                .proxy(address.to_owned(), true)
                .ratelimiter(None)
                .build(),
        ),
        application_id: twilight_model::id::Id::new(application_id),
        guild_id: twilight_model::id::Id::new(guild_id),
    }
}

/// A Discord API that answers command registration with whatever status
/// `status` holds when the request arrives, with an error body in
/// Discord's shape for failures.
pub(crate) async fn registration_server(
    status: Arc<AtomicU16>,
) -> anyhow::Result<(
    std::net::SocketAddr,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let (address, server, _) = counting_registration_server(status).await?;
    Ok((address, server))
}

async fn counting_registration_server(
    status: Arc<AtomicU16>,
) -> anyhow::Result<(
    std::net::SocketAddr,
    tokio::task::JoinHandle<std::io::Result<()>>,
    Arc<AtomicUsize>,
)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let router = Router::new().route(
        "/api/v10/applications/{application}/guilds/{guild}/commands",
        put(move || async move {
            counted.fetch_add(1, Ordering::SeqCst);
            let status = StatusCode::from_u16(status.load(Ordering::SeqCst))
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = if status.is_success() {
                json!([])
            } else {
                json!({"code": 50035, "message": "Invalid Form Body"})
            };
            (status, axum::Json(body))
        }),
    );
    Ok((
        address,
        tokio::spawn(async move { axum::serve(listener, router).await }),
        requests,
    ))
}

fn calls() -> anyhow::Result<super::InteractionCalls> {
    Ok(super::InteractionCalls::new(
        crate::client::test_client("http://127.0.0.1:1".to_owned())?,
        carillon_protocol::model::ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: None,
        },
    ))
}

fn interaction() -> Interaction {
    Interaction {
        kind: InteractionKind::Command,
        name: "model".to_owned(),
        values: Vec::new(),
        options: Value::Null,
        conversation: false,
    }
}

async fn reply(commands: &Module) -> anyhow::Result<String> {
    Ok(commands.invoke(calls()?, interaction()).await?.content)
}

/// A Discord API that reports each registration on `sent` and confirms it
/// only once `gate` grants a permit.
pub(crate) fn gated_registration(
    sent: tokio::sync::mpsc::UnboundedSender<Vec<Value>>,
    gate: Arc<tokio::sync::Semaphore>,
) -> Router {
    Router::new().route(
        "/api/v10/applications/{application}/guilds/{guild}/commands",
        put(move |axum::Json(definitions): axum::Json<Vec<Value>>| {
            let sent = sent.clone();
            let gate = gate.clone();
            async move {
                sent.send(definitions)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                gate.acquire()
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .forget();
                Ok::<_, StatusCode>(axum::Json(Vec::<Value>::new()))
            }
        }),
    )
}

/// `bytes` with a custom section appended so the module has exactly
/// `total` bytes. Custom sections do not change what a module does.
pub(crate) fn padded(mut bytes: Vec<u8>, total: usize) -> anyhow::Result<Vec<u8>> {
    fn leb128(mut value: usize, into: &mut Vec<u8>) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap_or_default();
            value >>= 7;
            if value == 0 {
                into.push(byte);
                return;
            }
            into.push(byte | 0x80);
        }
    }
    let name = b"padding";
    for width in 1..=5 {
        let size = total
            .checked_sub(bytes.len() + 1 + width)
            .ok_or_else(|| anyhow::anyhow!("module already exceeds {total} bytes"))?;
        let mut header = vec![0];
        leb128(size, &mut header);
        if header.len() != 1 + width {
            continue;
        }
        let payload = size - 1 - name.len();
        bytes.extend(header);
        bytes.push(u8::try_from(name.len())?);
        bytes.extend(name);
        bytes.resize(bytes.len() + payload, 0);
        anyhow::ensure!(bytes.len() == total);
        return Ok(bytes);
    }
    anyhow::bail!("cannot pad to {total} bytes")
}

/// Calls that submit through `router`, as the person in channel `400`,
/// with the reactions they show.
async fn submitting(
    router: Router,
    configure: impl FnOnce(&mut crate::config::DiscordConfig),
) -> anyhow::Result<(
    super::InteractionCalls,
    crate::delivery::Reactions,
    tokio::task::JoinHandle<std::io::Result<()>>,
)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let mut config = crate::conversations::tests::discord_config(
        crate::config::DiscordActivation::Always,
        crate::config::DiscordConversation::Thread,
    );
    configure(&mut config);
    let http = Arc::new(
        twilight_http::Client::builder()
            .token("test-only".to_owned())
            .proxy(address.to_string(), true)
            .ratelimiter(None)
            .build(),
    );
    let reactions =
        crate::delivery::Reactions::new(http.clone(), crate::config::Settings::new(config));
    let core = crate::client::test_client(format!("http://{address}"))?;
    let calls = super::InteractionCalls::new(
        core.clone(),
        carillon_protocol::model::ModelQuery {
            agent_id: "default".to_owned(),
            audience_id: Some("discord-100-400".to_owned()),
        },
    )
    .with_submissions(super::Submissions {
        lanes: serving_lanes(core, http.clone())?,
        http,
        reactions: reactions.clone(),
        origin: crate::conversations::InteractionOrigin {
            guild_id: "100".to_owned(),
            user_id: "200".to_owned(),
            channel_id: "400".to_owned(),
            route_channel_id: "300".to_owned(),
        },
        settings: reactions.settings().current(),
    });
    Ok((calls, reactions, server))
}

/// Lanes served by the provider's own dispatcher, in which no message of
/// the conversation is waiting.
fn serving_lanes(
    core: crate::client::CoreClient,
    http: Arc<twilight_http::Client>,
) -> anyhow::Result<crate::inbound::lanes::Lanes> {
    let directory = tempfile::tempdir()?;
    let participation = Arc::new(crate::conversations::Participation::load(
        directory.path().join("participation.json"),
    )?);
    let ingress = crate::inbound::tests::ingress(
        core,
        crate::conversations::tests::discord_config(
            crate::config::DiscordActivation::Always,
            crate::config::DiscordConversation::Thread,
        ),
        participation,
        http,
        directory.path(),
    )?;
    let (sender, receiver) = tokio::sync::mpsc::channel(crate::inbound::lanes::MAX_PENDING);
    tokio::spawn(async move {
        let _directory = directory;
        crate::inbound::lanes::dispatch(ingress, receiver).await;
    });
    Ok(crate::inbound::lanes::Lanes::new(sender))
}

/// The end of an interaction's native calls, were it starting now.
fn calls_until() -> tokio::time::Instant {
    tokio::time::Instant::now() + super::CALLS_LIMIT
}

fn submit_request() -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &carillon_discord_module::contract::HostRequest::Submit {
            text: "/compact keep decisions".to_owned(),
            intent: carillon_discord_module::contract::Intent::Command,
        },
    )?)
}

/// A module that answers every interaction as a Run click does: it asks
/// for the runtime's commands, then submits `/compact keep decisions`,
/// then replies `ran`.
fn running_module() -> anyhow::Result<Vec<u8>> {
    use carillon_discord_module::contract::{HostRequest, Invocation};
    let describe = describing(&[definition("command")]);
    let describe_length = serde_json::to_vec(&Invocation::Describe)?.len();
    let commands = serde_json::to_string(&HostRequest::Commands)?;
    let submit = String::from_utf8(submit_request()?)?;
    let reply = replying("ran", &[]);
    let card = showing("A test card");
    let text = format!(
        r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 2048) "{}")
  (data (i32.const 3072) "{}")
  (data (i32.const 4096) "{}")
  (data (i32.const 6144) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        ;; A card request begins {{"Card"
        (if (result i32) (i32.eq (i32.load8_u (i32.const 8194)) (i32.const 67))
          (then (global.set $output (i32.const 6144)) (i32.const {}))
          (else
            (drop (call $call (i32.const 2048) (i32.const {})))
            (drop (call $call (i32.const 3072) (i32.const {})))
            (global.set $output (i32.const 4096))
            (i32.const {}))))))
  (func (export "output") (result i32) (global.get $output)))"#,
        escape(&describe),
        escape(&commands),
        escape(&submit),
        escape(&reply),
        escape(&card),
        describe.len(),
        card.len(),
        commands.len(),
        submit.len(),
        reply.len(),
    );
    Ok(wat::parse_str(text)?)
}

/// A module that answers the cards a module is checked with before it
/// serves, whose origin turn is `turn`, with a valid card, and every other
/// card by running `on_card`: WAT that leaves the output's length, with
/// each of `data` at its address. Whatever it does, it installs.
pub(crate) fn other_cards(on_card: &str, data: &[(u32, &str)]) -> anyhow::Result<Vec<u8>> {
    let describe = describing(&[definition("model")]);
    let describe_length =
        serde_json::to_vec(&carillon_discord_module::contract::Invocation::Describe)?.len();
    let card = showing("checked");
    // The first byte of the origin turn's ID in `{"Card":{"job":{"origin":{"turn_id":"`.
    let turn = 32768 + 37;
    let text = format!(
        r#"(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (import "carillon" "take" (func $take (param i32 i32)))
  (memory (export "memory") 40)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  {}
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 32768))
  (func (export "invoke") (result i32)
    (local $reply i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else
        (if (result i32) (i32.eq (i32.load8_u (i32.const {turn})) (i32.const 116))
          (then (global.set $output (i32.const 4096)) (i32.const {}))
          (else {on_card})))))
  (func (export "output") (result i32) (global.get $output)))"#,
        escape(&describe),
        escape(&card),
        data.iter()
            .map(|(at, text)| format!("(data (i32.const {at}) \"{}\")", escape(text)))
            .collect::<Vec<_>>()
            .join(" "),
        describe.len(),
        card.len(),
    );
    Ok(wat::parse_str(text)?)
}

mod cards;

mod admission;
mod registration;
mod submissions;
