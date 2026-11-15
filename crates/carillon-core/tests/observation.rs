//! An observation against the real program: an installation `carillon init`
//! made, served by `carillon core` in a process of its own, read through
//! the interface client, and stopped as the service manager stops it.

use std::{
    io::Read as _,
    net::TcpListener,
    process::{Output, Stdio},
    time::Duration,
};

use anyhow::{Context as _, Result, bail, ensure};
use carillon_installation::config::{attachment_staging, attachment_store};
use carillon_interface::CoreClient;
use carillon_protocol::{
    model::{EventStatus, ExternalEvent, PROVIDER_HEADER, StagedAttachments, StagedFile},
    observation::{ConversationKey, Limit, OpenView, Opened, Progress, View},
};
use rustix::{
    fs::inotify,
    process::{Pid, Signal, kill_process},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::TcpStream,
    process::{Child, Command},
    time::Instant,
};

const CARILLON: &str = env!("CARGO_BIN_EXE_carillon");
/// The interface `carillon init --interface discord` registers.
const INTERFACE: &str = "discord-private";
/// Longer than any step here takes on a loaded machine.
const PATIENCE: Duration = Duration::from_secs(30);

/// An installation in temporary directories.
struct Installation {
    home: TempDir,
    _workspace: TempDir,
    port: u16,
    token: String,
}

impl Installation {
    fn create() -> Result<Self> {
        let (home, workspace) = (tempfile::tempdir()?, tempfile::tempdir()?);
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let made = std::process::Command::new(CARILLON)
            .arg("--home")
            .arg(home.path())
            .args(["init", "--interface", "discord", "--workspace"])
            .arg(workspace.path())
            .args(["--base-port", &port.to_string()])
            .output()?;
        succeeded(&made)?;
        let token = home.path().join(format!("credentials/{INTERFACE}.token"));
        Ok(Self {
            token: std::fs::read_to_string(token)?.trim().to_owned(),
            home,
            _workspace: workspace,
            port,
        })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn client(&self) -> Result<CoreClient> {
        CoreClient::new(self.url(), INTERFACE.to_owned(), self.token.clone())
    }

    /// Starts core on the installation and waits until it says it listens.
    async fn serve(&self) -> Result<Child> {
        let mut core = Command::new(CARILLON)
            .args(["core", "--config"])
            .arg(self.home.path().join("carillon.toml"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let said = core.stdout.take().context("core has no output")?;
        let mut said = BufReader::new(said).lines();
        let ready = async {
            while let Some(line) = said.next_line().await? {
                if line.contains("Carillon core ready") {
                    return Ok(());
                }
            }
            bail!("core ended before it served")
        };
        tokio::time::timeout(PATIENCE, ready)
            .await
            .context("core never said it was ready")??;
        // Read to its end, so core never waits to write to a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = said.next_line().await {} });
        Ok(core)
    }

    /// Accepts a message saying `text` in the conversation with `audience`.
    async fn say(&self, event_id: &str, audience: &str, text: String) -> Result<String> {
        let event = event(event_id, audience, text);
        Ok(self.client()?.submit_event(&event).await?.turn_id)
    }
}

fn event(event_id: &str, audience: &str, text: String) -> ExternalEvent {
    ExternalEvent {
        event_id: event_id.to_owned(),
        agent_id: "assistant".to_owned(),
        user_id: "someone".to_owned(),
        audience_id: audience.to_owned(),
        channel_id: "channel".to_owned(),
        delivery_target_id: "channel".to_owned(),
        feedback_target_id: event_id.to_owned(),
        content: text,
        native: serde_json::Value::Null,
        attachments: None,
    }
}

fn succeeded(output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Tells `core` to stop as the service manager does, and returns how long
/// it took to end. It must end cleanly.
async fn stop(mut core: Child) -> Result<Duration> {
    let pid = core.id().context("core already ended")?;
    let pid = Pid::from_raw(i32::try_from(pid)?).context("no process ID")?;
    let told = Instant::now();
    kill_process(pid, Signal::TERM)?;
    let ended = tokio::time::timeout(PATIENCE, core.wait())
        .await
        .context("core was still running")??;
    ensure!(ended.success(), "core ended with {ended}");
    Ok(told.elapsed())
}

fn open(audience: &str) -> Result<View> {
    Ok(View {
        conversations: Limit::default(),
        open: Some(OpenView {
            conversation: ConversationKey::new("assistant".to_owned(), audience.to_owned())?,
            turns: Limit::default(),
        }),
    })
}

#[tokio::test]
async fn an_interface_follows_its_view_until_core_stops_and_again_once_core_is_back() -> Result<()>
{
    let installation = Installation::create()?;
    let core = installation.serve().await?;
    let view = open("someone")?;
    let mut observation = installation.client()?.observe(&view).await?;
    let first = observation.next().await?.context("no first snapshot")?;
    assert!(first.conversations.is_empty());
    assert!(
        matches!(first.open, Some(Opened::Unrecorded { .. })),
        "{first:?}"
    );

    // No runtime serves this installation, so the turn stays queued.
    let turn_id = installation
        .say("one", "someone", "hello".to_owned())
        .await?;
    let queued = |snapshot: &carillon_protocol::observation::Snapshot| match &snapshot.open {
        Some(Opened::Recorded { turns, .. }) => turns
            .iter()
            .map(|turn| (turn.turn_id.clone(), turn.progress.clone()))
            .collect(),
        _ => Vec::new(),
    };
    let accepted = observation.next().await?.context("the stream ended")?;
    assert_eq!(queued(&accepted), [(turn_id.clone(), Progress::Queued)]);

    // Core ends the stream as it stops, and stops at once: its reader reads.
    stop(core).await?;
    assert_eq!(observation.next().await?, None);

    let core = installation.serve().await?;
    let mut observation = installation.client()?.observe(&view).await?;
    let again = observation.next().await?.context("no first snapshot")?;
    assert_eq!(again.conversations.len(), 1);
    assert_eq!(queued(&again), [(turn_id, Progress::Queued)]);
    stop(core).await?;
    Ok(())
}

#[tokio::test]
async fn core_stops_within_seconds_while_a_reader_reads_nothing() -> Result<()> {
    let installation = Installation::create()?;
    let core = installation.serve().await?;
    // A first snapshot of 12 MiB, more than the sockets between core and a
    // reader hold once the reader's buffer is made small.
    for index in 0..12 {
        let megabyte = "long ".repeat((1 << 20) / 5);
        installation
            .say(&format!("long {index}"), "someone", megabyte)
            .await?;
    }
    let mut reader = TcpStream::connect(("127.0.0.1", installation.port)).await?;
    socket2::SockRef::from(&reader).set_recv_buffer_size(4096)?;
    let request = format!(
        "GET /v1/observe?agent_id=assistant&audience_id=someone HTTP/1.1\r\n\
         host: 127.0.0.1\r\nauthorization: Bearer {}\r\n{PROVIDER_HEADER}: {INTERFACE}\r\n\r\n",
        installation.token
    );
    reader.write_all(request.as_bytes()).await?;
    // It reads the answer's head, so core has begun to write the snapshot,
    // and then never reads again.
    let mut head = Vec::new();
    while !head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let mut byte = [0];
        ensure!(reader.read(&mut byte).await? == 1, "core closed the stream");
        head.push(byte[0]);
    }
    ensure!(
        head.starts_with(b"HTTP/1.1 200 OK"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    let took = stop(core).await?;
    // Core waits two seconds for connections; the rest is a loaded machine.
    assert!(took < Duration::from_secs(10), "{took:?}");
    drop(reader);
    Ok(())
}

/// The bound on connections must not change what a stop does to a capture:
/// core gives the copy up at its next chunk, refuses the submission as
/// unavailable, and keeps nothing of it.
#[tokio::test]
async fn a_capture_in_flight_when_core_stops_is_given_up_and_its_message_refused() -> Result<()> {
    let installation = Installation::create()?;
    let core = installation.serve().await?;
    let state = installation.home.path().join("state");
    // A gibibyte takes core long enough to copy that it is still copying
    // when it is told to stop.
    let size = 1 << 30;
    let batch = attachment_staging(&state, INTERFACE).join("batch");
    std::fs::create_dir_all(&batch)?;
    std::fs::File::create(batch.join("0"))?.set_len(size)?;
    let mut message = event("with a file", "someone", "see the file".to_owned());
    message.attachments = Some(StagedAttachments {
        batch: "batch".to_owned(),
        files: vec![StagedFile {
            name: "large.bin".to_owned(),
            size,
            content_type: None,
        }],
    });
    // Core opens a staged file as it begins to capture it, and not before.
    let opening = inotify::init(inotify::CreateFlags::CLOEXEC)?;
    inotify::add_watch(&opening, batch.join("0"), inotify::WatchFlags::OPEN)?;
    let opened =
        tokio::task::spawn_blocking(move || std::fs::File::from(opening).read(&mut [0; 64]));
    let client = installation.client()?;
    let submitting = {
        let client = client.clone();
        tokio::spawn(async move { client.submit_event(&message).await })
    };
    tokio::time::timeout(PATIENCE, opened)
        .await
        .context("core never began to capture")???;

    let took = stop(core).await?;
    assert!(took < Duration::from_secs(10), "{took:?}");
    let refused = submitting
        .await?
        .err()
        .context("the message was accepted")?;
    assert_eq!(
        refused.status,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "{refused}"
    );

    let core = installation.serve().await?;
    let held = client.event_status("with a file").await?;
    assert!(matches!(held, EventStatus::Unknown), "{held:?}");
    let kept = std::fs::read_dir(attachment_store(&state))?.count();
    assert_eq!(kept, 0, "core kept part of a capture it gave up");
    stop(core).await?;
    Ok(())
}
