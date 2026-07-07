//! P3-GW1 (route A): a per-companion [`BrowserTool`] registry that lives in the
//! **main process** [`crate::deps::GatewayDeps`].
//!
//! ## Why a registry here (the route-A architecture)
//!
//! `GatewayDeps` is constructed in the main backend process; the bootstrap that
//! builds a session's `BrowserTool` runs in a *separate* agent/session process.
//! Route A does NOT migrate the engine across processes — it relies on the fact
//! that [`BrowserTool`] is **fully self-contained**:
//!
//! - `BrowserTool::with_data_dir(data_dir, headful).workspace(ws)` constructs the
//!   facade WITHOUT launching anything (the engine is built lazily inside the
//!   facade's own `Mutex` on the first action, and a launch failure is cached).
//! - so the gateway can simply OWN one `BrowserTool` per companion in the main
//!   process. Each companion's tool spins up its own in-process CDP engine on its
//!   first action — the same lazy mechanism the session bootstrap uses, just
//!   anchored in the gateway instead of a session.
//!
//! No cross-process engine handle, no engine-ownership migration: the registry is
//! the engine's owner for gateway-driven browsing.
//!
//! ## Per-companion engine slot + serialization (X5); shared browser IDENTITY
//!
//! [`BrowserTool::is_concurrency_safe`] is `false` — observe ⊥ act and per-target
//! actions must be serialized. The registry gives each companion key its own
//! [`tokio::sync::Mutex`]; [`BrowserRegistry::execute`] holds that mutex for the
//! whole tool call, so the same companion's `observe`/`act`/`navigate` never run
//! concurrently. Different companion keys hold different mutexes AND — crucially —
//! different Chrome processes with **distinct `--user-data-dir`s**: each slot's
//! `BrowserTool` self-allocates a process-unique profile dir
//! (`<data_dir>/profiles/<token>`, see `BrowserTool::profile_dir`), so two keys
//! browsing concurrently never share one user-data-dir and never hit Chromium's
//! process singleton (the old code passed only a per-key *workspace* — which the
//! engine used solely for downloads — while every slot's profile collapsed onto one
//! shared `<data_dir>/profile`, so concurrent keys collided; that is fixed).
//!
//! **Idle eviction (资源上界)**: the slot map was insert-only, so a finished
//! `conversation:<id>` kept a live Chrome forever and nothing bounded the number of
//! concurrent Chromes (Chromium's singleton used to be the accidental bound; once
//! profiles are per-instance that bound is gone). [`BrowserRegistry::sweep_idle`],
//! run opportunistically on every [`BrowserRegistry::slot`] access, evicts slots idle
//! past [`SLOT_IDLE_TTL_MS`] — dropping the `BrowserTool` kills its Chrome and cleans
//! its ephemeral profile dir.
//!
//! **User decision (去 per-pet 隔离): browser IDENTITY is globally shared.** The
//! per-companion *engine slot* (separate Chrome process + serialization mutex + a
//! per-instance profile dir) is kept — collapsing to one engine would turn
//! per-companion serialization into a global one, a behavior change we avoid. But
//! every slot points at the **same shared credential vault**
//! (`nomifun_secret::pet_vault_path` now ignores its key and routes to
//! `{data_dir}/browser-secrets/shared`), so `secret:NAME` / login / domain policy are
//! SHARED across companions and sessions (consistent with the unified-memory model).
//! Per-companion slots isolate the live Chrome process + its ephemeral profile, not
//! the persisted identity (login state flows through the shared encrypted vault).
//!
//! ## Workspace layout (默认 ④)
//!
//! Each key gets `{data_dir}/browser-profiles/{key}` as its workspace dir, so
//! gateway downloads (E4) land in a per-companion sandbox, never the user's real
//! Downloads. The key is the companion id when the caller carries one, else a
//! `conversation:<id>` fallback (a master/IM session driving a browser without a
//! companion binding still gets its own isolated tool).
//!
//! ## GW2 hook (left for the next task)
//!
//! Out-of-band approval of irreversible actions is GW2. GW1 wires the tool
//! exposure + execution path; the dispatch layer marks where an
//! `ApprovalTier::Irreversible` hit would be routed to the confirm channel. The
//! gateway-driven `BrowserTool` is constructed as a **non-bypassing** session
//! (`session_bypasses_approval = false`), so its own fail-closed redline gate does
//! NOT hard-deny — irreversible actions flow through to the engine today and will
//! be intercepted by the GW2 confirm hook once that task lands.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nomi_browser::{BrowserSecretSource, BrowserTool, OUT_OF_BAND_CONFIRMED_KEY};
use nomi_config::config::BrowserConfig;
use nomi_tools::Tool;
use nomi_types::tool::ToolResult;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

struct GatewayAutoApprovalGate;

#[async_trait::async_trait]
impl nomi_browser::BrowserApprovalGate for GatewayAutoApprovalGate {
    async fn request_approval(&self, _ask: nomi_browser::ApprovalAsk) -> nomi_browser::ApprovalDecision {
        nomi_browser::ApprovalDecision::Approve
    }
}

/// One companion's browser slot: a lazily-engined [`BrowserTool`] plus the mutex
/// that serializes that companion's tool calls (X5).
struct CompanionBrowser {
    tool: Arc<BrowserTool>,
    /// Per-companion serialization gate. Held for the duration of a single
    /// `execute` so observe/act/navigate for the SAME companion never overlap
    /// (the facade engine is `is_concurrency_safe = false`).
    lock: AsyncMutex<()>,
    /// **Slot idle tracking (并发/资源上界)**: wall-clock ms of the last `execute` on
    /// this slot. [`BrowserRegistry::sweep_idle`] evicts slots idle past
    /// [`SLOT_IDLE_TTL_MS`] so a finished `conversation:<id>` no longer keeps a live
    /// Chrome forever (the map was insert-only). Initialized to now on creation so a
    /// just-built slot is never swept before its first use.
    last_used_ms: std::sync::atomic::AtomicU64,
}

/// **P3-GW2**: a browser action held awaiting out-of-band approval. Stashed by the
/// dispatch layer when an action classifies as `ApprovalTier::Irreversible` in this
/// (auto-approving) gateway session, keyed by a synthetic `call_id` the phone/front-end
/// confirms. On approval, the registry re-issues `input` with the
/// [`OUT_OF_BAND_CONFIRMED_KEY`] sentinel injected so the facade's redline gate
/// releases it.
#[derive(Clone, Debug)]
pub struct PendingBrowserAction {
    /// The registry key (companion / conversation) the action belongs to — the
    /// engine it must run against once approved.
    pub key: String,
    /// The original, already-sanitized facade input (`{"action": "...", ...}`)
    /// WITHOUT any out-of-band sentinel (the caller-supplied one is stripped before
    /// stashing; the trusted one is injected only at resolve time).
    pub input: Value,
}

/// **P3-GW2**: cap on actions awaiting out-of-band approval across all keys. A
/// driving agent that keeps triggering irreversible actions without the user ever
/// approving must not be able to grow the store without bound; past this, the
/// dispatch layer fails closed (denies + tells the model to retry after the queue
/// drains) rather than stashing.
const MAX_PENDING: usize = 64;

/// The per-companion [`BrowserTool`] registry held by [`crate::deps::GatewayDeps`]
/// (route A). Clone-cheap: the inner map is behind an `Arc`.
#[derive(Clone)]
pub struct BrowserRegistry {
    /// Application data dir; per-companion workspaces hang under
    /// `{data_dir}/browser-profiles/{key}`.
    data_dir: PathBuf,
    /// Whether to request a visible (headful) window. The engine forces headless
    /// when no display is available regardless.
    headful: bool,
    /// PKG-1: bundled Chrome resource dir (Tauri resource dir). When `Some`, each
    /// lazily-built slot tool prefers `<bundled_dir>/chrome-<platform>/...` over the
    /// network download fallback. `None` (default / non-packaged) → unchanged
    /// behavior (env > data_dir > download).
    bundled_dir: Option<PathBuf>,
    /// companion-key → slot. A `std::sync::Mutex` guards only the (fast) map
    /// lookup/insert; the per-companion `AsyncMutex` inside the slot is what's
    /// held across an await-bound tool call.
    slots: Arc<std::sync::Mutex<HashMap<String, Arc<CompanionBrowser>>>>,
    /// **P3-GW2**: actions awaiting out-of-band approval, keyed by the synthetic
    /// `call_id` the phone/front-end confirms. An irreversible action in this
    /// auto-approving gateway session is stashed here (instead of forwarded) until
    /// the user approves it via `nomi_browser_confirm`. Bounded-ish: capped per the
    /// `MAX_PENDING` guard so a misbehaving agent cannot grow it without bound.
    pending: Arc<std::sync::Mutex<HashMap<String, PendingBrowserAction>>>,
    /// **P3-X2: machine-bound `encryption_key`** for loading the **shared** secret
    /// vault (`{data_dir}/browser-secrets/shared/secrets.json` — user decision: 去
    /// per-pet 键化, browser identity globally shared). When `Some`, each lazily-built
    /// slot tool gets a [`BrowserSecretSource`] pointing at that one shared vault so
    /// gateway-driven `secret:NAME` resolves (origin-gated) and the firewall domain
    /// allowlist is derived from the registered `allowed_origins` (裁决⑤) — shared
    /// across companions. `None` (the `default_for_browser_use` convenience ctor) →
    /// no secret source (empty store → `secret:NAME` fails closed, current behavior).
    secret_key: Option<[u8; 32]>,
}

impl BrowserRegistry {
    /// Build the registry from the browser config. Reads `headless` (inverted to
    /// `headful`) and the app data dir (same derivation as `BrowserTool::new`),
    /// under which each companion gets an isolated `browser-profiles/{key}`
    /// workspace. Constructs NO tools and launches NO browser — slots are created
    /// lazily on first use per companion.
    pub fn new(config: &BrowserConfig) -> Self {
        let data_dir = nomi_config::config::app_config_dir()
            .map(|d| d.join("browser-data"))
            .unwrap_or_else(|| std::env::temp_dir().join("nomi-browser-data"));
        Self {
            data_dir,
            headful: !config.headless,
            bundled_dir: None,
            slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            secret_key: None,
        }
    }

    /// **P3-X2**: set the machine-bound `encryption_key` so each companion's slot tool
    /// loads the **shared** secret vault (`{data_dir}/browser-secrets/shared/secrets.json`
    /// — 去 per-pet 键化, browser identity globally shared) — gateway-driven `secret:NAME`
    /// then resolves (origin-gated) and the firewall `allow_etld1` is derived from the
    /// registered `allowed_origins` (裁决⑤), shared across companions. Must be the app's
    /// `encryption_key` (the same one the registration endpoint encrypted with).
    pub fn with_secret_key(mut self, key: [u8; 32]) -> Self {
        self.secret_key = Some(key);
        self
    }

    /// **PKG-1**: set the bundled Chrome-for-Testing resource dir so each
    /// lazily-built companion slot tool prefers bundled chrome over the network
    /// download fallback. `None` → unchanged (env > data_dir > download).
    pub fn with_bundled_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.bundled_dir = dir;
        self
    }

    /// Convenience constructor for `nomifun-app`'s gateway wiring: build the
    /// registry with the default browser config so the app does not need a direct
    /// `nomi-config` dependency (the gateway already has one behind this feature).
    /// The engine forces headless when no display is available regardless, so the
    /// default (headful-requesting) config is the right gateway default.
    pub fn default_for_browser_use() -> Self {
        Self::new(&BrowserConfig::default())
    }

    /// Resolve the registry key for a caller. A companion binding scopes the
    /// browser to that companion (multi-companion isolation); a session without
    /// one (e.g. an IM master agent) gets a `conversation:<id>` key so it still
    /// has its own isolated tool. An empty/unknown caller falls back to a shared
    /// `"_default"` key.
    pub fn key_for(companion_id: Option<&str>, conversation_id: &str) -> String {
        match companion_id {
            Some(c) if !c.trim().is_empty() => c.trim().to_string(),
            _ if !conversation_id.trim().is_empty() => format!("conversation:{}", conversation_id.trim()),
            _ => "_default".to_string(),
        }
    }

    /// The per-companion workspace dir (`{data_dir}/browser-profiles/{key}`).
    /// Pure path join — no I/O (the engine materializes `downloads/` on demand).
    /// The key is **injectively** encoded (percent-encode of path separators/`:`) so a
    /// `conversation:<id>` (or any caller-influenced id) can never escape the profiles
    /// root AND two distinct keys never collapse onto one workspace (a lossy `:`→`_`
    /// map would let `conversation:5` and a literal `conversation_5` mix downloads).
    pub fn workspace_for(&self, key: &str) -> PathBuf {
        self.data_dir.join("browser-profiles").join(sanitize_key(key))
    }

    /// Get (or lazily create) the slot for a key. The `BrowserTool` is constructed
    /// but its engine is NOT launched (that happens lazily inside the facade on the
    /// first action). The gateway-driven tool is a **non-bypassing** session
    /// (`session_bypasses_approval = false`, `evaluate_full_power = false`): its own
    /// fail-closed redline gate does not hard-deny, leaving irreversible actions for
    /// the GW2 confirm hook (TODO at the dispatch layer).
    fn slot(&self, key: &str) -> Arc<CompanionBrowser> {
        let mut map = self.slots.lock().expect("browser registry slots poisoned");
        // 资源上界：顺手驱逐空闲过久的 slot（其 BrowserTool drop → 杀 Chrome + 清 ephemeral profile），
        // 使存活 Chrome 收敛到「近期活跃」的 key，绝不像旧的 insert-only 那样让已结束会话永久占一个 Chrome。
        let now = now_ms();
        map.retain(|_, s| !is_idle(s.last_used_ms.load(std::sync::atomic::Ordering::Relaxed), now, SLOT_IDLE_TTL_MS));
        if let Some(existing) = map.get(key) {
            return existing.clone();
        }
        let workspace = self.workspace_for(key);
        let mut tool = BrowserTool::with_data_dir(self.data_dir.clone(), self.headful)
            .workspace(workspace)
            .bundled_dir(self.bundled_dir.clone())
            .with_approval_gate(Arc::new(GatewayAutoApprovalGate));
        // P3-X2: give the slot tool the SHARED secret vault source so gateway-driven
        // `secret:NAME` resolves and the firewall allowlist is derived from the
        // registered allowed_origins (裁决⑤). User decision (去 per-pet 键化):
        // `pet_vault_path` now ignores `key` and routes every slot to the one shared
        // vault `{data_dir}/browser-secrets/shared`, so credentials/login/domain policy
        // are shared across all companions — the same shared vault the registration
        // endpoint and the session factory write to/read from.
        if let Some(secret_key) = self.secret_key {
            let vault_path = nomifun_secret::pet_vault_path(&self.data_dir, key);
            tool = tool.secret_source(BrowserSecretSource { vault_path, key: secret_key });
        }
        let slot = Arc::new(CompanionBrowser {
            tool: Arc::new(tool),
            lock: AsyncMutex::new(()),
            // 初始化为 now，使刚建的 slot 不会在首次使用前被上面的 sweep 误删。
            last_used_ms: std::sync::atomic::AtomicU64::new(now),
        });
        map.insert(key.to_string(), slot.clone());
        slot
    }

    /// **资源上界：驱逐空闲过久的 slot**（`now - last_used >= SLOT_IDLE_TTL_MS`）。被驱逐 slot 的
    /// [`CompanionBrowser`] 从 map 移除；当其最后一个 `Arc` 释放（无在途调用持有）时，`BrowserTool`
    /// drop → 引擎 drop → 杀 Chrome + 清 ephemeral profile。在途调用因持有 slot `Arc` 保活到调用结束，
    /// 故驱逐对并发调用安全。由 [`Self::slot`] 在每次访问时顺带调用；也供测试直接调用。
    pub fn sweep_idle(&self, now_ms: u64) {
        let mut map = self.slots.lock().expect("browser registry slots poisoned");
        map.retain(|_, s| {
            !is_idle(s.last_used_ms.load(std::sync::atomic::Ordering::Relaxed), now_ms, SLOT_IDLE_TTL_MS)
        });
    }

    /// Drive a browser tool call for `key`, serialized against that companion's
    /// other calls (X5: observe ⊥ act, per-target serial). `input` is the
    /// `BrowserTool` action object (`{"action": "...", ...}`). Returns the facade's
    /// [`ToolResult`] for the caller to render to JSON.
    pub async fn execute(&self, key: &str, input: Value) -> ToolResult {
        let slot = self.slot(key);
        // 记录活跃时刻（防被 sweep_idle 驱逐；见 CompanionBrowser::last_used_ms）。
        slot.last_used_ms.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        // Hold the per-companion mutex for the whole call so the same companion's
        // observe/act/navigate never run concurrently against one engine.
        let _guard = slot.lock.lock().await;
        slot.tool.execute(input).await
    }

    /// **并行浏览（DESIGN §26 P7 / §22 per-BrowserContext 可并发）**。批量跑 `(key, input)` 浏览器调用：
    /// **异 key** 并发（各 key 自有 Chrome 引擎 + 序列化锁,相互独立——浏览器**身份**仍经唯一共享 vault
    /// 全局共享,只有活进程 per-key）；**同 key** 仍经该 key 的 [`CompanionBrowser`] `lock` 串行
    /// （observe⊥act 成立）。结果**按输入序**返回（`join_all` 保序），调用方可一一对应。单个调用的错误作为
    /// 其 [`ToolResult`] 返回（引擎不可用 / 被拒），绝不中断整批。
    pub async fn execute_parallel(&self, calls: Vec<(String, Value)>) -> Vec<ToolResult> {
        // 复用串行 `execute`：异 key 持不同 `CompanionBrowser` 锁 → 真并发；同 key 第二个 future 在该锁上
        // 等第一个 → 串行,无交错。`join_all` 在当前任务上并发驱动所有 future 并**保输入序**返回。
        let futs = calls
            .into_iter()
            .map(|(key, input)| async move { self.execute(&key, input).await });
        futures::future::join_all(futs).await
    }

    /// **P3-GW2: classify an action's approval tier using the per-key facade's full
    /// runtime context** (its cached observe snapshot resolves a dangerous accname by
    /// `ref`). This is the AUTHORITATIVE classification the dispatch layer routes on —
    /// it sees the submit/Pay/删除 button signals a bare `classify_action` (without the
    /// snapshot) cannot. Pure read (no browser launch); creates the slot lazily if the
    /// caller classifies before its first execute (the tool, not the engine, is built).
    pub fn classify(&self, key: &str, action: &str, input: &Value) -> nomi_browser::ApprovalTier {
        self.slot(key).tool.classify_action_tier(action, input)
    }

    /// **P3-GW2**: stash an irreversible action awaiting out-of-band approval and
    /// return the synthetic `call_id` the phone/front-end will confirm. The `input`
    /// MUST already be sanitized of any caller-supplied out-of-band sentinel (the
    /// dispatch layer strips it before classifying). Returns `None` (so the caller
    /// fails closed and denies) when the pending store is at capacity — a
    /// misbehaving agent cannot grow it without bound.
    pub fn stash_pending(&self, key: &str, input: Value) -> Option<String> {
        let call_id = nomifun_common::generate_prefixed_id("browser_oob");
        let mut map = self.pending.lock().expect("browser registry pending poisoned");
        if map.len() >= MAX_PENDING {
            return None;
        }
        map.insert(
            call_id.clone(),
            PendingBrowserAction {
                key: key.to_string(),
                input,
            },
        );
        Some(call_id)
    }

    /// **P3-GW2**: remove and return a pending action by its `call_id`. `None` when
    /// the id is unknown (already resolved / never existed / expired). The caller
    /// (resolve path) treats `None` as "no such pending decision".
    pub fn take_pending(&self, call_id: &str) -> Option<PendingBrowserAction> {
        self.pending
            .lock()
            .expect("browser registry pending poisoned")
            .remove(call_id)
    }

    /// **P3-GW2**: how many actions are currently awaiting approval (diagnostics /
    /// the `MAX_PENDING` guard; also used by tests).
    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .expect("browser registry pending poisoned")
            .len()
    }

    /// **P3-GW2**: execute an out-of-band-APPROVED action — inject the trusted
    /// [`OUT_OF_BAND_CONFIRMED_KEY`] sentinel into the (sanitized) input so the
    /// facade's redline gate releases the irreversible action, then forward through
    /// the normal serialized `execute`. The sentinel is injected HERE (past the
    /// gateway trust boundary), never copied from caller input.
    pub async fn execute_confirmed(&self, key: &str, input: Value) -> ToolResult {
        self.execute(key, inject_out_of_band(input)).await
    }
}

/// **P3-GW2 [pure]: inject the trusted out-of-band sentinel** into a (sanitized)
/// action input so the facade's redline gate releases the irreversible action.
/// Called only by [`BrowserRegistry::execute_confirmed`] — past the gateway trust
/// boundary, after a real user approval. Pure (no I/O) so the injection is unit
/// testable without launching a browser.
fn inject_out_of_band(mut input: Value) -> Value {
    if let Some(obj) = input.as_object_mut() {
        obj.insert(OUT_OF_BAND_CONFIRMED_KEY.to_string(), Value::Bool(true));
    }
    input
}

/// Slot idle time-to-live (ms): a companion/conversation slot untouched this long is
/// evicted on the next registry access — its `BrowserTool` drops, killing the Chrome
/// process and (ephemeral) profile dir. Bounds live Chromes to *recently active* keys
/// (the map was previously insert-only → a finished `conversation:<id>` kept a live
/// Chrome forever). Generous so a single long browser op is never evicted mid-call.
const SLOT_IDLE_TTL_MS: u64 = 10 * 60 * 1000;

/// Wall-clock milliseconds since the Unix epoch. A backwards clock only makes a slot
/// look *younger*, never falsely idle. Used for slot idle tracking.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// **[pure] Is a slot idle (evictable)?** `now - last_used >= ttl`. Saturating so a
/// backwards clock (last_used in the "future") reads as fresh, never falsely idle.
fn is_idle(last_used_ms: u64, now_ms: u64, ttl_ms: u64) -> bool {
    now_ms.saturating_sub(last_used_ms) >= ttl_ms
}

/// **[pure] Injectively encode a registry key into a filesystem-safe dir segment.**
/// Percent-encodes `%` (first, so the escape is unambiguous) and the path separators
/// `/ \ :`. Injective: distinct keys always map to distinct segments (no lossy
/// `:`→`_` collapse that would let `conversation:5` and a literal `conversation_5`
/// share a downloads dir), and neutralizes separators so a key can't escape the root.
fn sanitize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            '\\' => out.push_str("%5C"),
            ':' => out.push_str("%3A"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a facade [`ToolResult`] into the gateway's JSON envelope. An error
/// result becomes `{"error": ...}`; a success result carries the text and any
/// images (base64 PNG) so a remote master agent can relay/inspect them.
pub fn tool_result_to_value(result: ToolResult) -> Value {
    if result.is_error {
        return json!({"error": result.content});
    }
    let mut payload = json!({"text": result.content});
    if !result.images.is_empty() {
        let imgs: Vec<Value> = result
            .images
            .iter()
            .map(|img| json!({"media_type": img.media_type, "data": img.data}))
            .collect();
        payload["images"] = Value::Array(imgs);
    }
    json!({"result": payload})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> BrowserRegistry {
        BrowserRegistry::new(&BrowserConfig::default())
    }

    #[test]
    fn is_idle_flags_slots_past_ttl() {
        // now - last_used >= ttl → idle。
        assert!(is_idle(1_000, 5_000, 3_000), "4s idle with 3s ttl → evictable");
        assert!(!is_idle(1_000, 3_000, 3_000) || is_idle(1_000, 4_000, 3_000)); // 边界附近
        assert!(!is_idle(4_000, 5_000, 3_000), "1s idle with 3s ttl → not evictable");
        // 时钟回拨（last_used 在未来）→ 饱和减为 0 → 视为新，不误删。
        assert!(!is_idle(9_000, 5_000, 3_000), "future last_used reads as fresh");
    }

    #[test]
    fn sweep_idle_evicts_stale_slots_keeps_fresh() {
        use std::sync::atomic::Ordering;
        let r = registry();
        // 建两个 slot（不启动 Chrome——slot() 只懒构 tool）。
        let stale = r.slot("companion_stale");
        let _fresh = r.slot("companion_fresh");
        // 把 stale 的 last_used 设到很久以前 → 应被 sweep 驱逐；fresh 保持 now。
        stale.last_used_ms.store(0, Ordering::Relaxed);
        r.sweep_idle(now_ms());
        let map = r.slots.lock().unwrap();
        assert!(!map.contains_key("companion_stale"), "stale slot must be evicted");
        assert!(map.contains_key("companion_fresh"), "fresh slot must survive");
    }

    #[test]
    fn key_prefers_companion_then_conversation_then_default() {
        assert_eq!(BrowserRegistry::key_for(Some("companion_x"), "5"), "companion_x");
        // Whitespace-only companion id is treated as absent.
        assert_eq!(BrowserRegistry::key_for(Some("  "), "5"), "conversation:5");
        assert_eq!(BrowserRegistry::key_for(None, "5"), "conversation:5");
        assert_eq!(BrowserRegistry::key_for(None, ""), "_default");
    }

    #[test]
    fn workspace_is_per_key_and_sanitized() {
        let r = registry();
        let a = r.workspace_for("companion_a");
        let b = r.workspace_for("companion_b");
        assert_ne!(a, b, "different companions must get different workspaces");
        assert!(a.ends_with(PathBuf::from("browser-profiles").join("companion_a")));
        // 键消毒是**单射**（percent-encode）：`conversation:5` 与字面 `conversation_5` 绝不塌成同一目录
        //（旧的 `:`→`_` 有损映射会塌，导致两会话下载/截图互相混淆）。
        let conv = r.workspace_for("conversation:5");
        assert!(
            conv.ends_with(PathBuf::from("browser-profiles").join("conversation%3A5")),
            "got {conv:?}"
        );
        assert_ne!(
            r.workspace_for("conversation:5"),
            r.workspace_for("conversation_5"),
            "key sanitization must be injective (distinct keys → distinct dirs)"
        );
        // 路径分隔符被中和（无 traversal），且仍单射。
        let evil = r.workspace_for("../../etc");
        assert!(
            evil.ends_with(PathBuf::from("browser-profiles").join("..%2F..%2Fetc")),
            "path separators in a key must be neutralized: {evil:?}"
        );
    }

    #[test]
    fn slot_is_stable_per_key_and_distinct_across_keys() {
        let r = registry();
        let a1 = r.slot("companion_a");
        let a2 = r.slot("companion_a");
        let b = r.slot("companion_b");
        // Same key → same slot (so the engine + its mutex are reused, not rebuilt).
        assert!(Arc::ptr_eq(&a1, &a2), "same key must reuse the same slot");
        // Different key → different slot (live Chrome process / mutex isolated per
        // companion; the persisted IDENTITY is still shared — see secret vault below).
        assert!(!Arc::ptr_eq(&a1, &b), "different keys must get isolated engine slots");
    }

    #[test]
    fn all_companions_resolve_the_same_shared_secret_vault() {
        // User decision (去 per-pet 键化): every companion key routes to the ONE shared
        // secret vault, so a secret registered for one companion is usable by every
        // companion's gateway-driven browser (shared browser identity).
        let r = registry();
        let shared_tail = std::path::Path::new("browser-secrets").join("shared").join("secrets.json");
        for key in ["companion_a", "companion_b", "conversation:5", "_default"] {
            let p = nomifun_secret::pet_vault_path(&r.data_dir, key);
            assert!(p.ends_with(&shared_tail), "key {key:?} must resolve the shared secret vault, got {p:?}");
        }
        // Distinct companion keys → identical shared vault path (the硬 evidence of sharing).
        assert_eq!(
            nomifun_secret::pet_vault_path(&r.data_dir, "companion_a"),
            nomifun_secret::pet_vault_path(&r.data_dir, "companion_b"),
            "去 per-pet 键化: two companions share one secret vault file"
        );
    }

    #[test]
    fn new_constructs_no_slots() {
        let r = registry();
        assert!(
            r.slots.lock().unwrap().is_empty(),
            "registry must not pre-create any companion slot (lazy per companion)"
        );
        assert_eq!(r.pending_count(), 0, "registry must start with no pending approvals");
    }

    #[test]
    fn gateway_browser_slot_installs_auto_approval_gate_for_egress() {
        let r = registry();
        let slot = r.slot("conversation:yolo");

        assert!(
            slot.tool.takeover_controller().enabled,
            "gateway browser slots need an approval gate so engine egress can approve without UI"
        );
    }

    // ── P3-GW2: pending out-of-band approval store ───────────────────────────

    #[test]
    fn stash_then_take_round_trips_the_pending_action() {
        let r = registry();
        let input = json!({"action": "click", "ref": "f0e3"});
        let call_id = r.stash_pending("companion_a", input.clone()).expect("under cap");
        assert!(call_id.starts_with("browser_oob"), "synthetic call_id prefix: {call_id}");
        assert_eq!(r.pending_count(), 1);

        let pending = r.take_pending(&call_id).expect("the just-stashed action");
        assert_eq!(pending.key, "companion_a");
        assert_eq!(pending.input, input);
        // Taken once → gone (a second take is None; a confirm cannot be replayed).
        assert!(r.take_pending(&call_id).is_none(), "take must be single-shot");
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn take_unknown_call_id_is_none() {
        let r = registry();
        assert!(r.take_pending("browser_oob_nope").is_none());
    }

    #[test]
    fn stash_keys_are_unique_per_action() {
        let r = registry();
        let a = r.stash_pending("k", json!({"action": "click"})).unwrap();
        let b = r.stash_pending("k", json!({"action": "click"})).unwrap();
        assert_ne!(a, b, "each stashed action must get its own call_id");
        assert_eq!(r.pending_count(), 2);
    }

    #[test]
    fn stash_fails_closed_at_capacity() {
        let r = registry();
        for _ in 0..MAX_PENDING {
            assert!(r.stash_pending("k", json!({"action": "click"})).is_some());
        }
        // At cap → None (the dispatch layer denies rather than growing unbounded).
        assert!(
            r.stash_pending("k", json!({"action": "click"})).is_none(),
            "stash must fail closed at MAX_PENDING"
        );
        assert_eq!(r.pending_count(), MAX_PENDING);
    }

    #[test]
    fn inject_out_of_band_sets_the_trusted_sentinel() {
        // execute_confirmed's pure core: the sentinel is injected here (past the trust
        // boundary), so the facade's out_of_band_confirmed reads true and the redline
        // gate releases the held irreversible action.
        let injected = inject_out_of_band(json!({"action": "click", "ref": "f0e3"}));
        assert_eq!(injected.get(OUT_OF_BAND_CONFIRMED_KEY).and_then(Value::as_bool), Some(true));
        assert_eq!(injected.get("action").and_then(Value::as_str), Some("click"));
        // Overwrites any pre-existing value to a strict bool true (never trusts input).
        let over = inject_out_of_band(json!({"action": "click", OUT_OF_BAND_CONFIRMED_KEY: "nope"}));
        assert_eq!(over.get(OUT_OF_BAND_CONFIRMED_KEY).and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn classify_builds_slot_and_returns_a_tier_without_launching() {
        // The dispatch layer's authoritative routing read: classify a benign read-only
        // action against a fresh key. The slot (tool) is built lazily but NO engine is
        // launched (pure read of the not-yet-existing snapshot → conservative tier).
        let r = registry();
        use nomi_browser::ApprovalTier;
        assert_eq!(
            r.classify("companion_c", "observe", &json!({"action": "observe"})),
            ApprovalTier::Info,
            "observe is read-only (Info)"
        );
        // A bare click with no cached snapshot → Exec (no accname to upgrade on).
        assert_eq!(
            r.classify("companion_c", "click", &json!({"action": "click", "ref": "f0e1"})),
            ApprovalTier::Exec
        );
        // press_key bare Enter → Irreversible even without a snapshot (args-derivable).
        assert_eq!(
            r.classify("companion_c", "press_key", &json!({"action": "press_key", "keys": "Enter"})),
            ApprovalTier::Irreversible
        );
    }

    #[test]
    fn error_result_maps_to_error_envelope() {
        let v = tool_result_to_value(ToolResult::error("boom"));
        assert_eq!(v.get("error").and_then(Value::as_str), Some("boom"));
        assert!(v.get("result").is_none());
    }

    #[test]
    fn text_result_maps_to_result_envelope() {
        let v = tool_result_to_value(ToolResult::text("Navigated to https://example.com"));
        assert_eq!(
            v.pointer("/result/text").and_then(Value::as_str),
            Some("Navigated to https://example.com")
        );
        assert_eq!(v.get("error"), None);
    }

    #[test]
    fn image_result_carries_base64_png() {
        let img = nomi_types::tool::ToolImage {
            media_type: "image/png".into(),
            data: "QUJD".into(), // base64("ABC")
        };
        let v = tool_result_to_value(ToolResult::text("Screenshot captured.").with_images(vec![img]));
        let arr = v.pointer("/result/images").and_then(Value::as_array).expect("images array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("media_type").and_then(Value::as_str), Some("image/png"));
        assert_eq!(arr[0].get("data").and_then(Value::as_str), Some("QUJD"));
    }

    // ── real-device end-to-end (needs a local/bundled chrome) ────────────────
    // GW1 round-trip through the registry: a gateway-driven navigate → observe
    // against a real Chromium, plus per-companion isolation (two keys → two
    // engines / user-data-dirs, distinct slots). Set NOMIFUN_CHROME_BINARY then:
    //   set NOMIFUN_CHROME_BINARY=C:\Program Files\Google\Chrome\Application\chrome.exe
    //   cargo nextest run -p nomifun-gateway --features browser-use --run-ignored all -E 'test(gateway_browser)'
    // Asserts the navigate result is non-error and the observe surfaces the
    // generation header + a frame-local `[ref=f0e…]` ref — i.e. a remote master
    // agent can drive a browser scoped to its companion. Clean up: no residual
    // chrome (the facade's engine Drop releases; the Builder kill_on_drop reaps).
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all -E 'test(gateway_browser)'"]
    async fn gateway_browser_navigate_then_observe_round_trip() {
        let r = registry();
        let key = BrowserRegistry::key_for(Some("companion_e2e"), "1");

        let nav = tool_result_to_value(
            r.execute(&key, json!({"action": "navigate", "url": "https://example.com"}))
                .await,
        );
        assert!(nav.get("error").is_none(), "navigate should succeed: {nav}");
        assert!(
            nav.pointer("/result/text").and_then(Value::as_str).is_some(),
            "navigate result should carry text: {nav}"
        );

        let obs = tool_result_to_value(r.execute(&key, json!({"action": "observe"})).await);
        let text = obs
            .pointer("/result/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("observe should carry text: {obs}"));
        assert!(text.contains("[browser observation"), "missing generation header: {text}");
        assert!(text.contains("[ref=f0e"), "missing a frame-local ref: {text}");

        // Isolation: a second companion gets a distinct slot (separate engine /
        // user-data-dir) — gateway-driven browsing is per-companion.
        let other = BrowserRegistry::key_for(Some("companion_other"), "2");
        assert!(
            !Arc::ptr_eq(&r.slot(&key), &r.slot(&other)),
            "different companions must get isolated browser slots"
        );
    }

    // ── P3-GW2 real-device: held-then-confirmed irreversible action round-trip ──
    // Drives the full out-of-band approval state machine against a real Chromium:
    // an irreversible action is stashed (NOT run), then the held action is approved
    // and runs via execute_confirmed (the trusted sentinel makes the facade release
    // it). Mirrors what `tools_browser::act` → `tools_browser::confirm` do, minus the
    // GatewayDeps wiring (which the #[ignore] gateway integration covers separately).
    //   set NOMIFUN_CHROME_BINARY=C:\Program Files\Google\Chrome\Application\chrome.exe
    //   cargo nextest run -p nomifun-gateway --features browser-use --run-ignored all -E 'test(gw2_confirmed)'
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all -E 'test(gw2_confirmed)'"]
    async fn gw2_confirmed_action_runs_held_then_approved() {
        let r = registry();
        let key = BrowserRegistry::key_for(Some("companion_gw2"), "1");

        // A data: URL with a real <form> whose submit button navigates on click.
        let page = "data:text/html,<form action='https://example.com/' method='get'>\
                    <button type='submit' id='go'>Pay now</button></form>";
        let nav = tool_result_to_value(r.execute(&key, json!({"action": "navigate", "url": page})).await);
        assert!(nav.get("error").is_none(), "navigate should succeed: {nav}");
        let obs = tool_result_to_value(r.execute(&key, json!({"action": "observe"})).await);
        let text = obs.pointer("/result/text").and_then(Value::as_str).unwrap_or("");
        // Find the submit button's ref from the snapshot.
        let r#ref = text
            .split("[ref=")
            .find(|seg| seg.to_lowercase().contains("pay") || seg.contains("button"))
            .and_then(|seg| seg.split(']').next())
            .unwrap_or("f0e1")
            .to_string();

        // GW2 gate decision: clicking a submit/Pay button is irreversible → stash it.
        let action = json!({"action": "click", "ref": r#ref});
        let call_id = r.stash_pending(&key, action.clone()).expect("under cap");
        assert_eq!(r.pending_count(), 1, "the irreversible click must be HELD, not run");

        // Approve: take the held action and run it confirmed (sentinel injected).
        let pending = r.take_pending(&call_id).expect("the held action");
        assert_eq!(pending.key, key);
        let result = tool_result_to_value(r.execute_confirmed(&key, pending.input).await);
        assert!(
            result.get("error").is_none(),
            "an approved (out-of-band-confirmed) irreversible action must RUN, not be Blocked: {result}"
        );
        assert_eq!(r.pending_count(), 0, "the pending action is consumed once resolved");
    }
}
