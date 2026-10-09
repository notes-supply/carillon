//! HTTP between an installation's own services, which listen on loopback.

use reqwest::{ClientBuilder, redirect};

/// A client for the installation's loopback services. Requests carry their
/// credentials, so they never go through a proxy the environment names, and
/// a redirect, which only a misconfigured or foreign listener would send, is
/// returned rather than followed elsewhere. Callers add their own timeouts.
pub fn client() -> ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(redirect::Policy::none())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead as _, BufReader, Write as _},
        net::TcpListener,
        process::Command,
        sync::mpsc,
        time::Duration,
    };

    use anyhow::Context as _;

    /// Set in the child process that makes the request.
    const PROBE: &str = "CARILLON_LOOPBACK_PROBE";

    /// A listener that answers every request with `response` and reports
    /// each request line it received.
    fn listener(response: &'static str) -> std::io::Result<(String, mpsc::Receiver<String>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        let (seen, requests) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_ok() {
                    let _ = seen.send(line.trim().to_owned());
                }
                let _ = (&stream).write_all(response.as_bytes());
            }
        });
        Ok((address, requests))
    }

    /// Proxies come from the environment, which only a fresh process can be
    /// given safely, so the request is made by this test in a child process.
    #[test]
    fn credentials_never_reach_an_environment_proxy_or_a_redirect_target() -> anyhow::Result<()> {
        if let Some(service) = std::env::var_os(PROBE) {
            let service = service
                .into_string()
                .map_err(|_| anyhow::anyhow!("address"))?;
            let status = tokio::runtime::Runtime::new()?.block_on(async {
                super::client()
                    .build()?
                    .get(format!("http://{service}/v1/status"))
                    .bearer_auth("secret")
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map(|response| response.status())
            })?;
            assert_eq!(status, reqwest::StatusCode::TEMPORARY_REDIRECT);
            return Ok(());
        }
        let (proxy, proxied) = listener("HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n")?;
        let (service, requests) = listener(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://192.0.2.1/stolen\r\n\
             content-length: 0\r\n\r\n",
        )?;
        let proxy = format!("http://{proxy}");
        let probe = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "loopback::tests::credentials_never_reach_an_environment_proxy_or_a_redirect_target",
            ])
            .env(PROBE, &service)
            .env("HTTP_PROXY", &proxy)
            .env("http_proxy", &proxy)
            .env("ALL_PROXY", &proxy)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()?;
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stdout)
        );
        assert_eq!(
            requests
                .recv_timeout(Duration::from_secs(5))
                .context("the service saw no request")?,
            "GET /v1/status HTTP/1.1"
        );
        assert!(proxied.try_recv().is_err(), "a request went to the proxy");
        Ok(())
    }
}
