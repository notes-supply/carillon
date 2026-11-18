//! A connection as a peer holds it: what it is refused, and when it is closed.

use std::net::SocketAddr;

use anyhow::ensure;
use carillon_protocol::observation::KEEP_ALIVE;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{Mutex, Notify},
};

use super::*;
use crate::serve::{HEADER_READ_TIMEOUT, MAX_CONNECTIONS, until_stopped};

#[tokio::test]
async fn a_browser_that_holds_its_connection_open_delays_a_stop_by_two_seconds_and_no_other_joins()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));

    let _browser = hold_an_upload(address, &token).await?;

    tokio::time::pause();
    let told = Instant::now();
    stop.notify_one();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let refused = TcpStream::connect(address)
        .await
        .err()
        .context("a connection was taken a second into the stop")?;
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
    serving.await??;
    // A timer is a millisecond coarse.
    let stopped = told.elapsed();
    ensure!(
        (Duration::from_secs(2)..=Duration::from_millis(2001)).contains(&stopped),
        "the stop took {stopped:?}"
    );
    Ok(())
}

/// A connection holding a message whose upload never comes: the provider
/// says it is ready for the body, and the browser sends none of it.
async fn hold_an_upload(address: SocketAddr, token: &str) -> Result<TcpStream> {
    let mut browser = TcpStream::connect(address).await?;
    let upload = format!(
        "PUT /api/conversations/assistant/0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b/messages/\
         m-0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b HTTP/1.1\r\nHost: {AUTHORITY}\r\n\
         Authorization: Bearer {token}\r\nContent-Type: multipart/form-data; boundary=b\r\n\
         Content-Length: 1000\r\nExpect: 100-continue\r\n\r\n"
    );
    browser.write_all(upload.as_bytes()).await?;
    let mut ready = [0; 25];
    tokio::time::timeout(Duration::from_secs(10), browser.read_exact(&mut ready))
        .await
        .context("the provider did not ask for an upload's body within ten seconds")??;
    assert_eq!(&ready, b"HTTP/1.1 100 Continue\r\n\r\n");
    Ok(browser)
}

/// Holding every connection takes most of the descriptors a process may
/// have, so tests that do take turns when they share a process.
static HOLDING_EVERY_CONNECTION: Mutex<()> = Mutex::const_new(());

async fn hold_every_connection(address: SocketAddr, token: &str) -> Result<Vec<TcpStream>> {
    // The cap's connections and the test's own ends of them pass the soft
    // descriptor limit a service manager gives a process by default.
    let descriptors = getrlimit(Resource::Nofile);
    let needed = u64::try_from(2 * MAX_CONNECTIONS + 64)?;
    if let Some(maximum) = descriptors.maximum {
        ensure!(
            maximum >= needed,
            "holding every connection takes about {needed} descriptors, past this process's \
             hard limit of {maximum}"
        );
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: descriptors.maximum,
            ..descriptors
        },
    )?;
    let mut browsers = Vec::with_capacity(MAX_CONNECTIONS);
    for _ in 0..MAX_CONNECTIONS {
        browsers.push(hold_an_upload(address, token).await?);
    }
    Ok(browsers)
}

#[tokio::test]
async fn a_connection_past_the_cap_is_unanswered_for_a_second_until_a_held_one_closes() -> Result<()>
{
    let _holding = HOLDING_EVERY_CONNECTION.lock().await;
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));
    let mut browsers = hold_every_connection(address, &token).await?;

    let mut past = TcpStream::connect(address).await?;
    past.write_all(
        format!("GET /health HTTP/1.1\r\nHost: {AUTHORITY}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await?;
    // Only a wait shows that nothing comes. Without the cap, the provider
    // answers within milliseconds.
    tokio::time::timeout(Duration::from_secs(1), past.read(&mut [0; 1]))
        .await
        .err()
        .context("a connection past the cap was answered while every connection was held")?;

    drop(browsers.pop());
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), past.read_to_end(&mut answer))
        .await
        .context("a connection past the cap was not answered once a held one closed")??;
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");

    drop(browsers);
    stop.notify_one();
    serving.await??;
    Ok(())
}

#[tokio::test]
async fn a_provider_holding_every_connection_it_serves_stops_within_two_seconds_of_a_paused_clock()
-> Result<()> {
    let _holding = HOLDING_EVERY_CONNECTION.lock().await;
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));
    let browsers = hold_every_connection(address, &token).await?;

    tokio::time::pause();
    let told = Instant::now();
    stop.notify_one();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .context("a provider holding every connection it serves did not stop")???;
    // A timer is a millisecond coarse.
    let stopped = told.elapsed();
    ensure!(
        (Duration::from_secs(2)..=Duration::from_millis(2001)).contains(&stopped),
        "the stop took {stopped:?}"
    );
    drop(browsers);
    Ok(())
}

#[tokio::test]
async fn an_upload_that_sends_nothing_for_30_seconds_of_a_paused_clock_loses_its_connection()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));
    let mut browser = hold_an_upload(address, &token).await?;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    // Only a wait shows that nothing comes, and the paused clock runs on
    // while the read waits.
    let early = tokio::time::timeout(Duration::from_millis(500), browser.read(&mut [0; 1])).await;
    ensure!(
        early.is_err(),
        "an upload was answered 29 seconds into a pause"
    );
    browser.write_all(b"--b\r\n").await?;
    let progressed = Instant::now();
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), browser.read_to_end(&mut answer))
        .await
        .context("an upload that sent nothing for 30 seconds kept its connection")??;
    // A timer is a millisecond coarse.
    let idle = progressed.elapsed();
    ensure!(
        (Duration::from_secs(30)..=Duration::from_millis(30_001)).contains(&idle),
        "an upload was closed {idle:?} after its last piece"
    );
    let answer = String::from_utf8_lossy(&answer);
    assert!(
        answer.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{answer}"
    );

    stop.notify_one();
    serving.await??;
    Ok(())
}

/// A browser that asks for the page's script and reads none of the answer.
/// Its receive buffer is small, so the provider soon has to wait to write.
async fn an_unread_download(address: SocketAddr) -> Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    socket.set_recv_buffer_size(4096)?;
    let mut browser = socket.connect(address).await?;
    browser
        .write_all(
            format!(
                "GET /index-Bq3nR7xK.js HTTP/1.1\r\nHost: {AUTHORITY}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    Ok(browser)
}

fn body_length(answer: &[u8]) -> Result<usize> {
    ensure!(
        answer.starts_with(b"HTTP/1.1 200 OK\r\n"),
        "{}",
        String::from_utf8_lossy(&answer[..answer.len().min(200)])
    );
    let head = answer
        .windows(4)
        .position(|end| end == b"\r\n\r\n")
        .context("the answer's head never ended")?;
    Ok(answer.len() - head - 4)
}

#[tokio::test]
async fn a_download_unread_for_60_seconds_of_a_paused_clock_is_cut_off_and_one_unread_for_59_is_not()
-> Result<()> {
    let fixture = Fixture::new().await?;
    // Far more than a connection's buffers hold.
    let script = 16 * 1024 * 1024;
    std::fs::File::options()
        .write(true)
        .open(fixture.web.site.release.join("index-Bq3nR7xK.js"))?
        .set_len(script)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));
    let mut paused = an_unread_download(address).await?;
    let mut stalled = an_unread_download(address).await?;

    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(59)).await;
    let mut answer = Vec::new();
    paused.read_to_end(&mut answer).await?;
    assert_eq!(
        body_length(&answer)?,
        usize::try_from(script)?,
        "a download unread for 59 seconds was cut off"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut answer = Vec::new();
    stalled.read_to_end(&mut answer).await?;
    let sent = body_length(&answer)?;
    assert!(
        sent < usize::try_from(script)?,
        "a download unread for 61 seconds was sent whole"
    );

    stop.notify_one();
    serving.await??;
    Ok(())
}

#[tokio::test]
async fn a_stream_its_page_reads_stays_open_through_two_minutes_of_a_paused_clock() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));
    let mut page = TcpStream::connect(address).await?;
    page.write_all(
        format!(
            "GET {STREAM} HTTP/1.1\r\nHost: {AUTHORITY}\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .as_bytes(),
    )
    .await?;

    tokio::time::pause();
    let opened = Instant::now();
    let mut read = [0; 4096];
    while opened.elapsed() < Duration::from_secs(120) {
        let length = tokio::time::timeout(2 * KEEP_ALIVE, page.read(&mut read))
            .await
            .with_context(|| {
                format!(
                    "the stream sent nothing for two keep-alives, {:?} after it opened",
                    opened.elapsed()
                )
            })??;
        ensure!(
            length > 0,
            "the stream closed {:?} after it opened",
            opened.elapsed()
        );
    }

    stop.notify_one();
    serving.await??;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_never_sends_its_request_is_closed_after_the_header_timeout() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));

    // One peer that opens a connection and sends nothing, and one that sends
    // part of a request line and no more.
    let silent = TcpStream::connect(address).await?;
    let mut slow = TcpStream::connect(address).await?;
    slow.write_all(b"GET /health HT").await?;
    tokio::task::yield_now().await;

    // Once the header timeout passes, the provider closes both without an
    // answer, so a read of each reaches the end rather than waiting forever.
    tokio::time::advance(HEADER_READ_TIMEOUT + Duration::from_secs(1)).await;
    for mut peer in [silent, slow] {
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut answer))
            .await
            .context("a connection was still open a header timeout after it stalled")??;
        assert_eq!(String::from_utf8_lossy(&answer), "");
    }

    stop.notify_one();
    serving.await??;
    Ok(())
}

#[tokio::test]
async fn a_request_s_line_and_headers_may_fill_64_kib_and_no_more() -> Result<()> {
    let fixture = Fixture::new().await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let stop = Arc::new(Notify::new());
    let asked_to_stop = {
        let stop = Arc::clone(&stop);
        async move { stop.notified().await }
    };
    let serving = tokio::spawn(until_stopped(
        listener,
        Arc::clone(&fixture.web),
        asked_to_stop,
    ));

    let line = format!("GET /health HTTP/1.1\r\nHost: {AUTHORITY}\r\nConnection: close\r\nX-Pad: ");
    let padded = |length: usize, end: &str| {
        let mut head = line.as_bytes().to_vec();
        head.resize(length - end.len(), b'p');
        head.extend_from_slice(end.as_bytes());
        head
    };
    let too_large = "HTTP/1.1 431 Request Header Fields Too Large\r\n";
    // A head that ends at 64 KiB is served, and one that ends a byte later,
    // or has not ended by then, is refused. Sent no further, a refused head
    // is read whole, so the provider closes without a reset that would lose
    // its answer. Each is sent on a new connection and on one that has
    // served a request.
    for (head, answered) in [
        (padded(64 * 1024, "\r\n\r\n"), "HTTP/1.1 200 OK\r\n"),
        (padded(64 * 1024 + 1, "\r\n\r\n"), too_large),
        (padded(64 * 1024, "p"), too_large),
    ] {
        for served in [false, true] {
            let mut browser = TcpStream::connect(address).await?;
            if served {
                browser
                    .write_all(
                        format!("HEAD /health HTTP/1.1\r\nHost: {AUTHORITY}\r\n\r\n").as_bytes(),
                    )
                    .await?;
                let mut first = Vec::new();
                while !first.ends_with(b"\r\n\r\n") {
                    first.push(browser.read_u8().await?);
                }
                assert!(first.starts_with(b"HTTP/1.1 200 OK\r\n"));
            }
            browser.write_all(&head).await?;
            let mut answer = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), browser.read_to_end(&mut answer))
                .await
                .with_context(|| format!("a {} byte head was not answered", head.len()))??;
            let answer = String::from_utf8_lossy(&answer);
            assert!(
                answer.starts_with(answered),
                "a {} byte head, served before {served}: {answer}",
                head.len()
            );
        }
    }

    stop.notify_one();
    serving.await??;
    Ok(())
}
