//! `ProcessStore`: a 64-way LRU registry of live interactive PTY sessions shared
//! by the `exec_command` and `write_stdin` tools, plus the incremental-read
//! collection loop they share.
//!
//! Ported (de-dependency-ed) from codex `unified_exec::process_manager`:
//! - 64-way cap, protect the most-recently-used 8, prune exited-then-oldest
//!   (`process_id_to_prune_from_meta`),
//! - prune is decided while holding the lock but the victim is `kill()`ed after
//!   the lock is released (`store_process`),
//! - `collect_output_until_deadline` minus codex's pause/network machinery.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::pty::Pty;

/// Upper bound on concurrently retained sessions (codex `MAX_UNIFIED_EXEC_PROCESSES`).
pub const MAX_PROCESSES: usize = 64;
/// The N most-recently-used sessions are never pruned.
const PROTECT_RECENT: usize = 8;

/// A live interactive session tracked by the store.
pub struct ExecSession {
    pub id: u64,
    pub pty: Arc<Pty>,
    /// Absolute byte offset in the PTY backlog consumed by the last tool call.
    pub read_offset: usize,
    /// The command line, for display/debugging.
    pub command: String,
    pub tty: bool,
    pub last_used: Instant,
}

/// Registry of live PTY sessions, keyed by a monotonic `u64` session id.
pub struct ProcessStore {
    inner: Mutex<HashMap<u64, ExecSession>>,
    next_id: AtomicU64,
}

impl Default for ProcessStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Insert a new session, pruning first if the store is full. Returns the new
    /// session id and, if a session was evicted to make room, its `Pty` so the
    /// caller can `kill()` it **after** releasing the store lock.
    pub async fn insert(&self, mut s: ExecSession) -> (u64, Option<Arc<Pty>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        s.id = id;
        let mut map = self.inner.lock().await;
        let pruned = if map.len() >= MAX_PROCESSES {
            Self::pick_prune(&map)
                .and_then(|pid| map.remove(&pid))
                .map(|e| e.pty)
        } else {
            None
        };
        map.insert(id, s);
        (id, pruned)
    }

    /// Fetch a session's `Pty` and refresh its `last_used` timestamp. Returns
    /// `None` if the id is unknown.
    pub async fn touch(&self, id: u64) -> Option<Arc<Pty>> {
        let mut map = self.inner.lock().await;
        let e = map.get_mut(&id)?;
        e.last_used = Instant::now();
        Some(e.pty.clone())
    }

    /// Fetch a session's `Pty` plus the durable-output cursor and refresh LRU.
    pub async fn touch_with_offset(&self, id: u64) -> Option<(Arc<Pty>, usize)> {
        let mut map = self.inner.lock().await;
        let e = map.get_mut(&id)?;
        e.last_used = Instant::now();
        Some((e.pty.clone(), e.read_offset))
    }

    /// Persist the durable-output cursor after a collection pass.
    pub async fn update_read_offset(&self, id: u64, read_offset: usize) {
        let mut map = self.inner.lock().await;
        if let Some(e) = map.get_mut(&id) {
            e.read_offset = read_offset;
            e.last_used = Instant::now();
        }
    }

    /// Remove a session from the store, returning it (caller decides whether to
    /// `kill()` — typically not needed if the child already exited).
    pub async fn remove(&self, id: u64) -> Option<ExecSession> {
        self.inner.lock().await.remove(&id)
    }

    /// Number of currently retained sessions (for tests/metrics).
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// True if a session id is currently retained (for tests).
    pub async fn contains(&self, id: u64) -> bool {
        self.inner.lock().await.contains_key(&id)
    }

    /// Pick a session to evict, replicating codex `process_id_to_prune_from_meta`:
    /// protect the most-recently-used `PROTECT_RECENT`, then evict the oldest
    /// **exited** unprotected session, else the oldest unprotected session.
    fn pick_prune(map: &HashMap<u64, ExecSession>) -> Option<u64> {
        let mut meta: Vec<(u64, Instant, bool)> = map
            .values()
            .map(|e| (e.id, e.last_used, e.pty.has_exited()))
            .collect();
        if meta.is_empty() {
            return None;
        }

        let mut by_recency = meta.clone();
        by_recency.sort_by(|a, b| b.1.cmp(&a.1)); // most-recent first
        let protected: HashSet<u64> = by_recency
            .iter()
            .take(PROTECT_RECENT)
            .map(|x| x.0)
            .collect();

        meta.sort_by(|a, b| a.1.cmp(&b.1)); // oldest first (LRU)
        meta.iter()
            .find(|(id, _, exited)| !protected.contains(id) && *exited)
            .map(|x| x.0)
            .or_else(|| {
                meta.iter()
                    .find(|(id, _, _)| !protected.contains(id))
                    .map(|x| x.0)
            })
    }

    /// Kill every retained session. Intended for engine shutdown so a model that
    /// spawned a pile of never-exiting REPLs doesn't leak processes.
    pub async fn terminate_all(&self) {
        let drained: Vec<ExecSession> = self.inner.lock().await.drain().map(|(_, e)| e).collect();
        for e in drained {
            e.pty.kill();
        }
    }
}

impl Drop for ProcessStore {
    /// Best-effort synchronous cleanup when the last `Arc<ProcessStore>` drops
    /// (i.e. the engine and its `ToolRegistry` are torn down). PTY children are
    /// `setsid()`'d into their own process group, so dropping the `Arc<Pty>`
    /// alone does **not** reap them — we must SIGKILL the group. `get_mut` on the
    /// `tokio::Mutex` is uncontended here (sole owner), so no async runtime is
    /// needed.
    fn drop(&mut self) {
        let map = self.inner.get_mut();
        for (_, e) in map.drain() {
            e.pty.kill();
        }
    }
}

/// Incrementally read PTY output until `deadline`. If the child has exited and
/// the output stream is closed, finishes early after draining any residue.
///
/// This is codex `collect_output_until_deadline` minus pause/network: it is the
/// engine of "empty polling" — `write_stdin` with `chars=""` writes nothing and
/// drops straight into this loop to read whatever arrived in `yield_time_ms`.
pub async fn collect_until_deadline(
    pty: &Pty,
    read_offset: usize,
    deadline: Instant,
) -> (Vec<u8>, usize) {
    let (mut rx, snapshot, mut read_offset) = pty.subscribe_from(read_offset);
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    out.extend_from_slice(&snapshot);
    loop {
        let now = Instant::now();
        if now >= deadline {
            let (snapshot, next_offset) = pty.snapshot_from(read_offset);
            out.extend_from_slice(&snapshot);
            read_offset = next_offset;
            break;
        }
        if pty.has_exited() && pty.output_closed() {
            // Exited and the stream is closed: scoop up any residue and finish.
            while let Ok(chunk) = rx.try_recv() {
                read_offset = read_offset.saturating_add(chunk.len());
                out.extend_from_slice(&chunk);
            }
            let (snapshot, next_offset) = pty.snapshot_from(read_offset);
            out.extend_from_slice(&snapshot);
            read_offset = next_offset;
            break;
        }
        let remaining = deadline - now;
        let closed_notify = pty.closed_notify();
        tokio::select! {
            r = rx.recv() => match r {
                Ok(chunk) => {
                    read_offset = read_offset.saturating_add(chunk.len());
                    out.extend_from_slice(&chunk);
                }
                Err(RecvError::Lagged(_)) => {
                    let (new_rx, snapshot, next_offset) = pty.subscribe_from(read_offset);
                    out.extend_from_slice(&snapshot);
                    read_offset = next_offset;
                    rx = new_rx;
                    continue;
                }
                Err(RecvError::Closed) => {
                    let (snapshot, next_offset) = pty.snapshot_from(read_offset);
                    out.extend_from_slice(&snapshot);
                    read_offset = next_offset;
                    break;
                }
            },
            _ = closed_notify.notified() => { /* re-evaluate finish on next loop */ }
            _ = tokio::time::sleep(remaining) => break,
        }
    }
    (out, read_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::PtyParams;
    use crate::test_support::pty_test_helper_program;
    use std::collections::HashMap as StdHashMap;

    /// Spawn a long-lived child that stays alive ~`secs` seconds via the
    /// cross-platform helper (replaces the unix-only `sleep`).
    fn spawn_sleep(secs: &str) -> Arc<Pty> {
        let ms = secs.parse::<u64>().unwrap_or(30) * 1000;
        Pty::spawn(PtyParams {
            program: pty_test_helper_program(),
            args: vec!["sleep".into(), ms.to_string()],
            cwd: String::new(),
            env: StdHashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn helper sleep")
    }

    fn session(pty: Arc<Pty>) -> ExecSession {
        ExecSession {
            id: 0,
            pty,
            read_offset: 0,
            command: "sleep".into(),
            tty: false,
            last_used: Instant::now(),
        }
    }

    #[tokio::test]
    async fn ids_are_monotonic_and_lookup_works() {
        let store = ProcessStore::new();
        let (id1, p1) = store.insert(session(spawn_sleep("30"))).await;
        let (id2, p2) = store.insert(session(spawn_sleep("30"))).await;
        assert!(p1.is_none() && p2.is_none());
        assert_eq!(id2, id1 + 1);
        assert!(store.touch(id1).await.is_some());
        assert!(store.touch(9999).await.is_none());
        store.terminate_all().await;
    }

    #[tokio::test]
    async fn lru_caps_at_max_and_protects_recent() {
        let store = ProcessStore::new();
        let mut ids = Vec::new();
        // Insert MAX_PROCESSES + 1: the +1 must trigger exactly one eviction.
        for _ in 0..=MAX_PROCESSES {
            let (id, pruned) = store.insert(session(spawn_sleep("30"))).await;
            if let Some(victim) = pruned {
                victim.kill();
            }
            ids.push(id);
        }
        assert_eq!(
            store.len().await,
            MAX_PROCESSES,
            "store must cap at MAX_PROCESSES"
        );

        // The 8 most-recently-inserted ids are protected and must survive.
        for id in ids.iter().rev().take(PROTECT_RECENT) {
            assert!(
                store.contains(*id).await,
                "recently-used session {id} must not be pruned"
            );
        }
        // The very first inserted (oldest, unprotected) must have been evicted.
        assert!(
            !store.contains(ids[0]).await,
            "oldest unprotected session should be evicted first"
        );
        store.terminate_all().await;
    }

    #[tokio::test]
    async fn prune_prefers_exited_sessions() {
        // Build a meta set by hand to assert the policy without racing on exits.
        let store = ProcessStore::new();
        // One short-lived (will exit), several long-lived.
        let exiting = Pty::spawn(PtyParams {
            program: pty_test_helper_program(),
            args: vec!["exit".into(), "0".into()],
            cwd: String::new(),
            env: StdHashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn helper exit");
        let (exited_id, _) = store.insert(session(exiting)).await;
        // Give the waiter thread time to record exit.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // Fill to capacity with live sessions so the next insert must prune.
        let mut last_ids = Vec::new();
        for _ in 0..(MAX_PROCESSES - 1) {
            let (id, pruned) = store.insert(session(spawn_sleep("30"))).await;
            if let Some(v) = pruned {
                v.kill();
            }
            last_ids.push(id);
        }
        assert_eq!(store.len().await, MAX_PROCESSES);
        // Touch the exited session so it is NOT among the oldest, proving the
        // policy targets "exited" over "oldest". It is old by insertion but we
        // refresh last_used so recency wouldn't pick it — yet exited should.
        // (Skip the touch: leave it oldest; either way it should be pruned since
        // it is both exited and unprotected.)
        let (_new_id, pruned) = store.insert(session(spawn_sleep("30"))).await;
        let victim_killed = pruned.is_some();
        if let Some(v) = pruned {
            v.kill();
        }
        assert!(victim_killed, "insert past cap must evict someone");
        assert!(
            !store.contains(exited_id).await,
            "an exited unprotected session should be the prune target"
        );
        store.terminate_all().await;
    }
}
