//! Hosting of the replaceable command module: loading it, learning which
//! commands it defines, registering those with Discord, and running each
//! interaction through it with the native model operations it may ask for.

use carillon_protocol::models::ModelState;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use carillon_discord_commands::contract::{
    CONTRACT, Description, HostReply, HostRequest, Interaction, Invocation, Outcome, Reply,
};
use carillon_installation::filesystem::atomic_write;
use carillon_protocol::model::{ModelQuery, ModelScope, ModelUpdate};
use carillon_wasm::{Host, Module, Native};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};
use twilight_http::Client as DiscordHttp;
use twilight_model::{
    application::command::Command,
    id::{
        Id,
        marker::{ApplicationMarker, GuildMarker},
    },
};

use crate::provider::CoreClient;

/// The whole invocation, including every native call it makes.
const DEADLINE: Duration = Duration::from_secs(10);
/// Description is pure computation; a module that needs longer is broken.
const DESCRIBE_DEADLINE: Duration = Duration::from_secs(2);

/// The Discord application the provider acts as, once the token is known,
/// and the guild whose commands it owns.
pub struct DiscordApp {
    pub http: Arc<DiscordHttp>,
    pub application_id: Id<ApplicationMarker>,
    pub guild_id: Id<GuildMarker>,
}

/// The native operations one interaction may perform: model state and
/// selection for the agent and, inside a conversation, for that audience.
pub struct ModelCalls {
    core: CoreClient,
    query: ModelQuery,
}

impl ModelCalls {
    pub const fn new(core: CoreClient, query: ModelQuery) -> Self {
        Self { core, query }
    }
}

/// Native calls for invocations that must not perform any.
enum Calls {
    Refused,
    Model(ModelCalls),
}

impl Native for Calls {
    async fn call(&mut self, request: Vec<u8>) -> Vec<u8> {
        let reply = match self {
            Self::Refused => HostReply::Error("native calls are not available".to_owned()),
            Self::Model(calls) => calls.answer(&request).await,
        };
        serde_json::to_vec(&reply).unwrap_or_default()
    }
}

impl ModelCalls {
    async fn answer(&self, request: &[u8]) -> HostReply {
        let outcome = match serde_json::from_slice::<HostRequest>(request) {
            Ok(HostRequest::ModelState) => self.core.model_state(&self.query).await,
            Ok(HostRequest::SetModel {
                scope,
                selection,
                expected,
            }) => {
                if scope == ModelScope::Conversation && self.query.audience_id.is_none() {
                    return HostReply::Error(
                        "There is no conversation here to set a model for.".to_owned(),
                    );
                }
                self.core
                    .set_model(&ModelUpdate {
                        agent_id: self.query.agent_id.clone(),
                        audience_id: self.query.audience_id.clone(),
                        scope,
                        selection,
                        expected,
                    })
                    .await
            }
            Err(error) => Err(format!("invalid host request: {error}")),
        };
        match outcome {
            Ok(value) => match serde_json::from_value::<ModelState>(value) {
                Ok(state) => HostReply::ModelState(state),
                Err(_) => {
                    HostReply::Error("The runtime reported an unusable model state.".to_owned())
                }
            },
            Err(message) => HostReply::Error(message),
        }
    }
}

/// One immutable module and the definitions it described. In-flight calls
/// retain their generation when a replacement starts serving.
struct Generation {
    module: Module,
    definitions: Vec<Value>,
}

/// What Discord last confirmed, bound to the application and guild it was
/// registered with; a record for another target does not count.
#[derive(Deserialize, Serialize)]
struct Record {
    application_id: Id<ApplicationMarker>,
    guild_id: Id<GuildMarker>,
    commands: Vec<Value>,
}

/// The module in use, its command definitions, and the record of what was
/// last registered with Discord.
pub struct Commands {
    host: Host<Calls>,
    module_path: PathBuf,
    record_path: PathBuf,
    current: Mutex<Arc<Generation>>,
    reload: tokio::sync::Mutex<()>,
}

#[derive(Serialize)]
pub struct ReloadReport {
    commands: usize,
    changed: bool,
    registered: bool,
}

#[derive(Debug)]
pub enum ReloadError {
    Prepare(anyhow::Error),
    Register(anyhow::Error),
}

impl Commands {
    /// Reloads one generation at a time, including remote registration. A
    /// failed replacement leaves current invocations and definitions alone.
    pub async fn reload(&self, app: Option<&DiscordApp>) -> Result<ReloadReport, ReloadError> {
        let _reload = self.reload.lock().await;
        let candidate = self.prepare().await.map_err(ReloadError::Prepare)?;
        let mut report = ReloadReport {
            commands: candidate.definitions.len(),
            changed: self.changed(&candidate),
            registered: false,
        };
        if let Some(app) = app
            && !self.registered(app, &candidate.definitions)
        {
            self.register(app, &candidate.definitions)
                .await
                .map_err(ReloadError::Register)?;
            report.registered = true;
        }
        self.activate(candidate);
        Ok(report)
    }

    /// Loads the module at `module_path` and learns its commands. The record
    /// of registered commands lives at `record_path`.
    pub async fn load(module_path: PathBuf, record_path: PathBuf) -> Result<Self> {
        let host = Host::new()?;
        let candidate = prepare(&host, &module_path).await?;
        Ok(Self {
            host,
            module_path,
            record_path,
            current: Mutex::new(Arc::new(candidate)),
            reload: tokio::sync::Mutex::new(()),
        })
    }

    /// Re-reads the module file without putting it into service; a broken
    /// file changes nothing. The candidate serves once [`Self::activate`]
    /// accepts it, after any registration its definitions need.
    async fn prepare(&self) -> Result<Generation> {
        prepare(&self.host, &self.module_path).await
    }

    /// Whether the candidate defines other commands than the module in use.
    fn changed(&self, candidate: &Generation) -> bool {
        self.current().definitions != candidate.definitions
    }

    /// Replaces the module in use with the candidate.
    fn activate(&self, candidate: Generation) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(candidate);
        info!(path = %self.module_path.display(), "command module reloaded");
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.current().definitions.clone()
    }

    /// Whether Discord already holds `definitions` for this application and
    /// guild.
    pub fn registered(&self, app: &DiscordApp, definitions: &[Value]) -> bool {
        fs::read(&self.record_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            .is_some_and(|record| {
                record.application_id == app.application_id
                    && record.guild_id == app.guild_id
                    && record.commands == definitions
            })
    }

    /// Replaces the guild's commands with `definitions`, invalidating the
    /// disposable cache first and recording only Discord's confirmation.
    /// Cache persistence cannot veto an already-confirmed registration.
    pub async fn register(&self, app: &DiscordApp, definitions: &[Value]) -> Result<()> {
        let commands = definitions
            .iter()
            .map(command)
            .collect::<Result<Vec<_>>>()?;
        // This cache must not attest to the old definitions if Discord
        // applies the new ones but recording its confirmation fails.
        match fs::remove_file(&self.record_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("invalidate command registration cache"),
        }
        app.http
            .interaction(app.application_id)
            .set_guild_commands(app.guild_id, &commands)
            .await
            .context("register Discord guild commands")?;
        let record = Record {
            application_id: app.application_id,
            guild_id: app.guild_id,
            commands: definitions.to_vec(),
        };
        let record_result = (|| -> Result<()> {
            if let Some(parent) = self.record_path.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write(&self.record_path, &serde_json::to_vec(&record)?, 0o600)?;
            Ok(())
        })();
        if let Err(error) = record_result {
            // Discord confirmed the candidate. A cache failure cannot undo
            // that mutation or justify keeping the previous module active.
            warn!(%error, path = %self.record_path.display(), "commands registered but confirmation cache could not be written; registration will be retried");
        }
        info!(
            commands = commands.len(),
            "Discord guild commands registered"
        );
        Ok(())
    }

    /// Runs one interaction through the module.
    pub async fn invoke(&self, calls: ModelCalls, interaction: Interaction) -> Result<Reply> {
        let generation = self.current();
        let payload = serde_json::to_vec(&Invocation::Interaction(interaction))?;
        match outcome(
            &self.host,
            &generation.module,
            Calls::Model(calls),
            &payload,
            DEADLINE,
        )
        .await?
        {
            Outcome::Reply(reply) => Ok(reply),
            Outcome::Failure(message) => bail!("command module failed: {message}"),
            Outcome::Description(_) => {
                bail!("command module answered an interaction with a description")
            }
        }
    }
}

impl Commands {
    fn current(&self) -> Arc<Generation> {
        Arc::clone(&self.current.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

async fn prepare(host: &Host<Calls>, path: &Path) -> Result<Generation> {
    let bytes =
        fs::read(path).with_context(|| format!("read command module {}", path.display()))?;
    let module = host
        .load(&bytes)
        .with_context(|| format!("load command module {}", path.display()))?;
    let payload = serde_json::to_vec(&Invocation::Describe)?;
    let description = match outcome(host, &module, Calls::Refused, &payload, DESCRIBE_DEADLINE)
        .await?
    {
        Outcome::Description(description) => description,
        Outcome::Reply(_) | Outcome::Failure(_) => bail!("command module did not describe itself"),
    };
    let Description { contract, commands } = description;
    if contract != CONTRACT {
        bail!("command module speaks contract {contract}; this provider speaks {CONTRACT}");
    }
    for definition in &commands {
        command(definition)?;
    }
    Ok(Generation {
        module,
        definitions: commands,
    })
}

async fn outcome(
    host: &Host<Calls>,
    module: &Module,
    calls: Calls,
    payload: &[u8],
    deadline: Duration,
) -> Result<Outcome> {
    let output = host
        .invoke(module, calls, payload, deadline)
        .await
        .map_err(anyhow::Error::from)?;
    serde_json::from_slice(&output).context("decode command module outcome")
}

/// A command definition as Discord's registration API expects it. Modules
/// describe commands without registration metadata, which Discord assigns.
fn command(definition: &Value) -> Result<Command> {
    let Value::Object(fields) = definition else {
        bail!("command definition must be an object");
    };
    let mut fields = fields.clone();
    fields.entry("version").or_insert(json!("1"));
    serde_json::from_value(Value::Object(fields)).context("invalid command definition")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicU16, Ordering},
        },
    };

    use axum::{Router, http::StatusCode, routing::put};
    use carillon_discord_commands::contract::{Interaction, InteractionKind};
    use serde_json::{Value, json};

    use super::{Commands, DiscordApp};

    /// A module that answers `Describe` with `description` and every other
    /// invocation with `reply`, written to `path` in binary form.
    pub(crate) fn write_module(path: &Path, description: &str, reply: &str) -> anyhow::Result<()> {
        let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
        let describe_length =
            serde_json::to_vec(&carillon_discord_commands::contract::Invocation::Describe)?.len();
        let text = format!(
            r#"(module
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (global $output (mut i32) (i32.const 0))
  (data (i32.const 1024) "{}")
  (data (i32.const 4096) "{}")
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 8192))
  (func (export "invoke") (result i32)
    (if (result i32) (i32.eq (global.get $length) (i32.const {describe_length}))
      (then (global.set $output (i32.const 1024)) (i32.const {}))
      (else (global.set $output (i32.const 4096)) (i32.const {}))))
  (func (export "output") (result i32) (global.get $output)))"#,
            escape(description),
            escape(reply),
            description.len(),
            reply.len(),
        );
        std::fs::write(path, wat::parse_str(text)?)?;
        Ok(())
    }

    pub(crate) fn describing(commands: &[Value]) -> String {
        json!({"Description": {"contract": carillon_discord_commands::contract::CONTRACT, "commands": commands}}).to_string()
    }

    pub(crate) fn replying(content: &str, components: &[Value]) -> String {
        json!({"Reply": {"content": content, "components": components}}).to_string()
    }

    pub(crate) fn definition(name: &str) -> Value {
        json!({"type": 1, "name": name, "description": "A test command"})
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            put(move || async move {
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
        ))
    }

    fn calls() -> super::ModelCalls {
        super::ModelCalls::new(
            crate::provider::tests::core_client("http://127.0.0.1:1".to_owned()),
            carillon_protocol::model::ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        )
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

    #[tokio::test]
    async fn loading_checks_the_contract_and_definitions() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        let reply = replying("unused", &[]);
        for description in [
            json!({"Description": {"contract": carillon_discord_commands::contract::CONTRACT + 1, "commands": []}}).to_string(),
            describing(&[json!({"name": "missing-type"})]),
            reply.clone(),
            "not json".to_owned(),
        ] {
            write_module(&module, &description, &reply)?;
            assert!(
                Commands::load(module.clone(), record.clone())
                    .await
                    .is_err(),
                "accepted {description}"
            );
        }
        write_module(&module, &describing(&[definition("model")]), &reply)?;
        let commands = Commands::load(module, record).await?;
        assert_eq!(commands.definitions(), vec![definition("model")]);
        assert!(!commands.registered(&app("127.0.0.1:1", 1, 100), &commands.definitions()));
        Ok(())
    }

    #[tokio::test]
    async fn candidates_serve_only_once_activated() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        write_module(
            &module,
            &describing(&[definition("model")]),
            &replying("one", &[]),
        )?;
        let commands = Commands::load(module.clone(), record).await?;
        std::fs::write(&module, b"\0asm broken")?;
        assert!(commands.prepare().await.is_err());
        assert_eq!(commands.definitions(), vec![definition("model")]);
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "one"
        );
        write_module(
            &module,
            &describing(&[definition("model")]),
            &replying("two", &[]),
        )?;
        let candidate = commands.prepare().await?;
        assert!(!commands.changed(&candidate));
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "one"
        );
        commands.activate(candidate);
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "two"
        );
        write_module(
            &module,
            &describing(&[definition("model"), definition("other")]),
            &replying("three", &[]),
        )?;
        let candidate = commands.prepare().await?;
        assert!(commands.changed(&candidate));
        assert_eq!(candidate.definitions.len(), 2);
        assert_eq!(commands.definitions().len(), 1);
        commands.activate(candidate);
        assert_eq!(commands.definitions().len(), 2);
        Ok(())
    }

    fn gated_registration(
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

    #[tokio::test]
    async fn overlapping_reloads_register_and_activate_in_order() -> anyhow::Result<()> {
        use std::time::Duration;
        use tokio::sync::{Semaphore, mpsc};

        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        write_module(
            &module,
            &describing(&[definition("initial")]),
            &replying("initial", &[]),
        )?;
        let commands = Commands::load(module.clone(), record).await?;
        let retained = commands.current();
        let (sent, mut received) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = app(&listener.local_addr()?.to_string(), 1, 100);
        let router = gated_registration(sent, release.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        write_module(
            &module,
            &describing(&[definition("first")]),
            &replying("first", &[]),
        )?;
        let first = commands.reload(Some(&target));
        tokio::pin!(first);
        let registration = tokio::select! {
            _ = &mut first => anyhow::bail!("reload completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "first");
        write_module(
            &module,
            &describing(&[definition("second")]),
            &replying("second", &[]),
        )?;
        let second = commands.reload(Some(&target));
        tokio::pin!(second);
        // Poll both operations while registration is blocked. The second
        // must not send a request or activate; ordinary invocations still work.
        tokio::select! {
            _ = &mut first => anyhow::bail!("first activated before confirmation"),
            _ = &mut second => anyhow::bail!("second bypassed the first reload"),
            request = received.recv() => anyhow::bail!("overlapping registration: {request:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "initial"
        );
        release.add_permits(1);
        assert!(first.await.is_ok());
        assert_eq!(commands.definitions(), vec![definition("first")]);
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "first"
        );
        let registration = tokio::select! {
            _ = &mut second => anyhow::bail!("second completed before confirmation"),
            request = received.recv() => request.ok_or_else(|| anyhow::anyhow!("registration closed"))?,
        };
        assert_eq!(registration[0]["name"], "second");
        release.add_permits(1);
        assert!(second.await.is_ok());
        assert_eq!(commands.definitions(), vec![definition("second")]);
        assert!(commands.registered(&target, &commands.definitions()));
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "second"
        );
        // A previously captured generation keeps both its code and metadata.
        assert_eq!(retained.definitions, vec![definition("initial")]);
        let payload = serde_json::to_vec(
            &carillon_discord_commands::contract::Invocation::Interaction(interaction()),
        )?;
        let old = super::outcome(
            &commands.host,
            &retained.module,
            super::Calls::Refused,
            &payload,
            super::DEADLINE,
        )
        .await?;
        assert_eq!(
            old,
            carillon_discord_commands::contract::Outcome::Reply(
                carillon_discord_commands::contract::Reply {
                    content: "initial".to_owned(),
                    components: vec![],
                }
            )
        );
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn registration_is_recorded_for_its_target_once_confirmed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        write_module(
            &module,
            &describing(&[definition("model")]),
            &replying("unused", &[]),
        )?;
        let commands = Commands::load(module, record.clone()).await?;
        let definitions = commands.definitions();
        let status = Arc::new(AtomicU16::new(200));
        let (address, server) = registration_server(status.clone()).await?;
        let target = app(&address.to_string(), 1, 100);

        // A record in the shape an earlier provider wrote names no target.
        std::fs::write(&record, serde_json::to_vec(&definitions)?)?;
        assert!(!commands.registered(&target, &definitions));

        commands.register(&target, &definitions).await?;
        assert!(commands.registered(&target, &definitions));
        assert!(!commands.registered(&target, &[definition("other")]));
        assert!(!commands.registered(&app(&address.to_string(), 1, 101), &definitions));
        assert!(!commands.registered(&app(&address.to_string(), 2, 100), &definitions));

        // Neither a rejection nor an unanswered request counts as confirmed.
        let other = vec![definition("other")];
        for code in [400, 500] {
            status.store(code, Ordering::SeqCst);
            assert!(commands.register(&target, &other).await.is_err(), "{code}");
            assert!(!commands.registered(&target, &definitions));
            assert!(!commands.registered(&target, &other));
            assert!(!record.exists());
        }
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn confirmed_reload_activates_even_when_recording_fails() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        write_module(
            &module,
            &describing(&[definition("old")]),
            &replying("old", &[]),
        )?;
        let commands = Commands::load(module.clone(), record.clone()).await?;
        let blocked = record.clone();
        let router = Router::new().route(
            "/api/v10/applications/{application}/guilds/{guild}/commands",
            put(move || {
                let blocked = blocked.clone();
                async move {
                    // The remote mutation succeeded; the local destination
                    // becomes unwritable before its confirmation is recorded.
                    std::fs::create_dir(&blocked).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                    Ok::<_, StatusCode>(axum::Json(Vec::<Value>::new()))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let target = app(&listener.local_addr()?.to_string(), 1, 100);
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        write_module(
            &module,
            &describing(&[definition("new")]),
            &replying("new", &[]),
        )?;
        assert!(commands.reload(Some(&target)).await.is_ok());
        assert_eq!(commands.definitions(), vec![definition("new")]);
        assert_eq!(
            commands.invoke(calls(), interaction()).await?.content,
            "new"
        );
        assert!(!commands.registered(&target, &commands.definitions()));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn model_calls_preserve_the_conditional_write() -> anyhow::Result<()> {
        use axum::Json;
        use carillon_discord_commands::contract::{HostReply, HostRequest};
        use carillon_protocol::model::{ModelQuery, ModelUpdate};
        use carillon_protocol::{model::ModelScope as Scope, models::Selection};
        let selection = Selection {
            provider: "fixture".to_owned(),
            model: "A".to_owned(),
            options: std::collections::BTreeMap::new(),
        };
        let expected = selection.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route(
            "/v1/model",
            put(move |Json(update): Json<ModelUpdate>| {
                let expected = expected.clone();
                async move {
                    assert_eq!(update.expected, Some(expected));
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"error": "changed while applying"})),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let calls = super::ModelCalls::new(
            crate::provider::tests::core_client(format!("http://{address}")),
            ModelQuery {
                agent_id: "default".to_owned(),
                audience_id: None,
            },
        );
        let reply = calls
            .answer(&serde_json::to_vec(&HostRequest::SetModel {
                scope: Scope::Agent,
                selection: Some(selection.clone()),
                expected: Some(selection),
            })?)
            .await;
        assert_eq!(reply, HostReply::Error("changed while applying".to_owned()));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn interactions_receive_the_module_reply_or_a_failure() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let module = directory.path().join("commands.wasm");
        let record = directory.path().join("commands.json");
        let button = json!({"type": 1, "components": [{"type": 2, "style": 2, "custom_id": "x", "label": "X"}]});
        write_module(
            &module,
            &describing(&[]),
            &replying("hello", std::slice::from_ref(&button)),
        )?;
        let commands = Commands::load(module.clone(), record.clone()).await?;
        let reply = commands.invoke(calls(), interaction()).await?;
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.components, vec![button]);
        for output in [
            json!({"Failure": "no such command"}).to_string(),
            describing(&[]),
            "{broken".to_owned(),
        ] {
            write_module(&module, &describing(&[]), &output)?;
            commands.activate(commands.prepare().await?);
            assert!(
                commands.invoke(calls(), interaction()).await.is_err(),
                "accepted {output}"
            );
        }
        Ok(())
    }
}
