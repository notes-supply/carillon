//! Ports for an installation that core's unit and integration tests serve.

use std::{
    hash::{BuildHasher as _, RandomState},
    net::{Ipv4Addr, TcpListener},
};

use anyhow::{Context as _, Result, bail};
use carillon_installation::bundled::Bundled;

/// A loopback port with the ports of every bundled program above it, all
/// free when probed. The probe releases them before the test uses them, so
/// they lie below the kernel's ephemeral range, where no socket is given a
/// port in between, and below the 128 ports under it that the installer and
/// persistence tests take, which may run at the same time.
pub(crate) fn available_base_port() -> Result<u16> {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")?;
    let ephemeral: u16 = range
        .split_whitespace()
        .next()
        .context("the ephemeral port range is empty")?
        .parse()?;
    let above = Bundled::ALL
        .map(Bundled::port_offset)
        .into_iter()
        .max()
        .unwrap_or_default();
    let span = ephemeral
        .checked_sub(128 + 1024 + above)
        .filter(|&span| span > 0)
        .with_context(|| format!("no free ports below the ephemeral range {}", range.trim()))?;
    for _ in 0..10 {
        let base = 1024 + u16::try_from(RandomState::new().hash_one(()) % u64::from(span))?;
        let probed = (base..=base + above)
            .map(|port| TcpListener::bind((Ipv4Addr::LOCALHOST, port)))
            .collect::<std::io::Result<Vec<_>>>();
        if probed.is_ok() {
            return Ok(base);
        }
    }
    bail!("could not find a free run of loopback ports below {ephemeral}")
}
