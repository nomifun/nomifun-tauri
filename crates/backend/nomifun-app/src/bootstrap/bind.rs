//! `bind_with_fallback` — the one place port failover lives.
//!
//! Lifted from `desktop::bind_lan` (which only did preferred→ephemeral) and
//! upgraded with a bounded deterministic scan so a busy fixed port falls back to
//! a *predictable* neighbour (`:25809` after `:25808`) before resorting to an
//! OS-assigned ephemeral one. Shared by the desktop LAN listener, `nomifun-web`,
//! and the `nomicore` bin so all three hosts fail over identically.
//!
//! The function returns the *actually-bound* port alongside the listener: a
//! client cannot reach a port that moved unless something announces it
//! (`port.json` / stdout), so every caller must surface the returned port.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

/// Consecutive ports above `preferred` to try before accepting an ephemeral one.
pub const SCAN_SPAN: u16 = 16;

/// Bind `host:preferred`; if occupied, scan `preferred+1 ..= preferred+SCAN_SPAN`;
/// if all occupied, bind an ephemeral port. Returns `(actual_port, listener)`.
pub async fn bind_with_fallback(host: IpAddr, preferred: u16) -> Result<(u16, TcpListener)> {
    // 1. Preferred port — the common, deterministic case.
    if let Ok(l) = TcpListener::bind((host, preferred)).await {
        let port = l.local_addr()?.port();
        return Ok((port, l));
    }
    // 2. Bounded deterministic scan — keeps the chosen port predictable across
    //    restarts (an operator can guess `:25809` after `:25808`), unlike a pure
    //    ephemeral jump. `checked_add` guards the u16 boundary near 65535.
    for offset in 1..=SCAN_SPAN {
        let Some(candidate) = preferred.checked_add(offset) else {
            break;
        };
        if let Ok(l) = TcpListener::bind((host, candidate)).await {
            let port = l.local_addr()?.port();
            return Ok((port, l));
        }
    }
    // 3. Ephemeral last resort — never fail purely because fixed ports are busy.
    let l = TcpListener::bind((host, 0))
        .await
        .context("failed to bind on preferred, scanned, and ephemeral ports")?;
    let port = l.local_addr()?.port();
    Ok((port, l))
}

/// File name under the data dir announcing the actually-bound address.
pub const PORT_FILE: &str = "port.json";

/// On-disk announcement of the bound address. The only way a browser, launcher,
/// or AutoWork loop learns a port that fell back off its default — there is no
/// injection channel for the fixed-port (web / nomicore) hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortAnnouncement {
    pub host: String,
    pub port: u16,
    pub channel: String,
    pub pid: u32,
}

/// Atomically write the announcement to `{data_dir}/port.json` (tmp + rename,
/// mirroring `provision::state::store`).
pub fn write_port_file(data_dir: &Path, announcement: &PortAnnouncement) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(announcement).map_err(std::io::Error::other)?;
    let final_path = data_dir.join(PORT_FILE);
    let tmp_path = data_dir.join(format!("{PORT_FILE}.tmp"));
    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)
}

/// Announce the bound address: print one stdout line and best-effort write
/// `port.json`. Never fatal — the server is already listening, so a failed
/// announcement is logged, not propagated.
pub fn announce_bound_port(data_dir: &Path, host: &str, port: u16) {
    let announcement = PortAnnouncement {
        host: host.to_string(),
        port,
        channel: crate::channel::channel().to_string(),
        pid: std::process::id(),
    };
    println!(
        "nomifun: listening on {host}:{port} (channel {})",
        announcement.channel
    );
    if let Err(e) = write_port_file(data_dir, &announcement) {
        tracing::warn!(error = %e, "failed to write port.json (non-fatal; server is up)");
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use tokio::net::TcpListener;

    use super::bind_with_fallback;

    const LH: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    static PORT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Discover a currently-free loopback port by binding ephemeral and dropping.
    async fn free_port() -> u16 {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn binds_preferred_when_free() {
        let _guard = PORT_TEST_LOCK.lock().await;
        for _ in 0..32 {
            let p = free_port().await;
            let (got, l) = bind_with_fallback(LH, p).await.unwrap();
            if got == p {
                assert_eq!(got, l.local_addr().unwrap().port());
                return;
            }
            drop(l);
            tokio::task::yield_now().await;
        }
        panic!("must bind the requested port when it is free");
    }

    #[tokio::test]
    async fn falls_back_off_occupied_preferred() {
        let _guard = PORT_TEST_LOCK.lock().await;
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let busy = occupied.local_addr().unwrap().port();
        let (got, l) = bind_with_fallback(LH, busy).await.unwrap();
        assert_ne!(got, busy, "must not return the occupied port");
        assert_eq!(
            got,
            l.local_addr().unwrap().port(),
            "reported port matches the listener"
        );
    }

    #[tokio::test]
    async fn reports_listener_actual_port() {
        let _guard = PORT_TEST_LOCK.lock().await;
        let (got, l) = bind_with_fallback(LH, 0).await.unwrap();
        assert_eq!(got, l.local_addr().unwrap().port());
    }

    #[test]
    fn port_file_roundtrips_and_leaves_no_tmp() {
        use super::{PORT_FILE, PortAnnouncement, write_port_file};
        let tmp = tempfile::TempDir::new().unwrap();
        let a = PortAnnouncement {
            host: "127.0.0.1".into(),
            port: 8788,
            channel: "dev".into(),
            pid: 42,
        };
        write_port_file(tmp.path(), &a).unwrap();

        let bytes = std::fs::read(tmp.path().join(PORT_FILE)).expect("port.json must be written");
        let back: PortAnnouncement = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, a);
        assert!(
            !tmp.path().join(format!("{PORT_FILE}.tmp")).exists(),
            "tmp sidecar must be renamed away"
        );
    }
}
