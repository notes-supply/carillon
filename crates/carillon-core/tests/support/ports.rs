//! Ports for an installation that core's unit and integration tests serve.

use std::{
    hash::{BuildHasher as _, RandomState},
    net::{Ipv4Addr, TcpListener},
};

use anyhow::{Context as _, Result};
use carillon_installation::bundled::Bundled;

/// A loopback port with the ports of every bundled program above it, all
/// free when probed and all ones an unprivileged process may bind. The probe
/// releases them before the test uses them, so they lie below the kernel's
/// ephemeral range, where no socket is given a port in between, and below the
/// 128 ports under it that the installer and persistence tests take, which
/// may run at the same time.
pub(crate) fn available_base_port() -> Result<u16> {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")?;
    let ephemeral: u16 = range
        .split_whitespace()
        .next()
        .context("the ephemeral port range is empty")?
        .parse()?;
    let unprivileged: u16 =
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")?
            .trim()
            .parse()?;
    // Containers often set the floor to 0, and port 0 would ask for an ephemeral port.
    let first = unprivileged.max(1024);
    let above = Bundled::ALL
        .map(Bundled::port_offset)
        .into_iter()
        .max()
        .unwrap_or_default();
    let span = ephemeral
        .checked_sub(128 + above)
        .and_then(|ceiling| ceiling.checked_sub(first))
        .filter(|&span| span > 0)
        .with_context(|| {
            format!(
                "no free ports from {first} below the ephemeral range {}",
                range.trim()
            )
        })?;
    let probe = || -> Result<u16> {
        let base = first + u16::try_from(RandomState::new().hash_one(()) % u64::from(span))?;
        (base..=base + above)
            .map(|port| TcpListener::bind((Ipv4Addr::LOCALHOST, port)))
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(base)
    };
    for _ in 1..10 {
        if let Ok(base) = probe() {
            return Ok(base);
        }
    }
    probe()
        .with_context(|| format!("could not find a free run of loopback ports below {ephemeral}"))
}
