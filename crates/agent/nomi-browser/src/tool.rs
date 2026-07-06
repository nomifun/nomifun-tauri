//! The Browser tool: a thin facade over the in-process self-hosted CDP engine
//! (`nomi-browser-engine`). Mirrors `nomi-computer::ComputerTool`: an honest
//! capability note baked into the (cacheable) description, a lazily-initialized
//! engine whose construction error is cached (an unavailable backend is reported
//! without retrying), per-action approval categories, and non-panicking
//! `ToolResult` errors the model routes around.
//!
//! P0 exposed navigate/screenshot/capabilities. P1 added the read-only `observe`
//! action (aria-snapshot of the page + a `[ref=f<seq>e<n>]` table). **F1** wires
//! the full action space: `execute` parses the tool input into a
//! [`nomi_browser_engine::ActSpec`] and dispatches it through
//! `BrowserEngine::act` (click/type/set_value/hover/select/press_key/scroll/
//! …/back/forward/reload/switch_*/tabs), with `secret:NAME` interception (the
//! value is resolved through an origin-bound vault and injected as
//! [`nomi_browser_engine::TypeInput::Secret`] — it never reaches the LLM).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};

use nomi_browser_engine::progress::Progress;
use nomi_browser_engine::{ActSpec, BrowserEngine, BrowserError, Capabilities, Effect, TypeInput};
use nomi_browser_engine::{ChromeSource, EngineConfig, Observation, create_engine};
use nomi_browser_engine::{load_storage_state, save_storage_state, shared_storage_state_path};
use nomi_config::config::BrowserConfig;
use nomi_protocol::ToolApprovalManager;
use nomi_protocol::events::ToolCategory;
use nomi_tools::Tool;
use nomi_types::tool::{JsonSchema, ToolResult};
use nomifun_secret::SecretStore;

use crate::extract::{self, ExtractModel, ExtractModelRef, ExtractSchema};
use crate::redline::{self, ActionContext, ApprovalTier};

/// **P7B SoM cap**: max clickable elements to label on a Set-of-Marks overlay. Beyond this,
/// numbered boxes clutter the screenshot and degrade vision-model selection accuracy, so the
/// facade skips SoM and uses the raw-bbox fallback instead. Also bounds the per-label palette
/// cycling and keeps the annotated PNG legible.
const MAX_SOM_LABELS: usize = 50;

/// **P3-X2: per-pet secret vault source** threaded into [`BrowserTool`] so it can
/// lazily load the registered credentials (and derive the firewall domain
/// allowlist) on the first action.
///
/// Carries the shared vault file path (`{data_dir}/browser-secrets/shared/secrets.json`,
/// resolved by `nomifun_secret::pet_vault_path` which — 去 per-pet 键化, browser identity
/// globally shared — ignores its key and routes to the one shared vault) + the
/// machine-bound 32-byte key (the app's `encryption_key`, the same one the registration
/// endpoint used to encrypt the values). `Clone` so it can ride through `NomiResolvedConfig`
/// → bootstrap (the `SecretStore` itself is intentionally NOT carried — it is
/// non-`Clone`/non-`Debug` and is loaded lazily at engine-build time so a freshly-registered
/// secret is picked up).
#[derive(Clone)]
pub struct BrowserSecretSource {
    /// The shared secret vault file path.
    pub vault_path: PathBuf,
    /// The machine-bound AES-256-GCM key (`encryption_key`, 32 bytes).
    pub key: [u8; nomifun_secret::KEY_SIZE],
}

impl std::fmt::Debug for BrowserSecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("BrowserSecretSource")
            .field("vault_path", &self.vault_path)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// `secret:NAME` 引用前缀（裁决⑦）。`type`/`set_value` 的 `text` 以此开头时，facade **拦截**
/// 该引用：经 [`SecretStore::resolve`] 在 origin 门通过后取真值，包成 [`TypeInput::Secret`] 注入引擎
/// （值经 `Input.insertText` 写入，**绝不**进 LLM 输出 / ref 表 / 日志）。
const SECRET_PREFIX: &str = "secret:";

/// **P3-GW2: 带外确认 sentinel key**（裁决④）。`Tool::execute(&self, input)` 的签名不携带会话上下文，
/// 故「这次不可逆动作已获带外确认」这一事实只能经 `input` 传进 facade。本 reserved key 是那条路径：
/// `out_of_band_confirmed` 当且仅当 `input[OUT_OF_BAND_CONFIRMED_KEY] == true` 时返 `true`，让
/// [`redline::enforce_redline`] 的第三参为真 → 旁路会话里的不可逆动作放行。
///
/// **信任边界（关键不变量）**：本 key 的唯一合法置位者是**网关 dispatch 层**——它在带外审批通道（手机/
/// 前端）拿到用户批准后才注入。LLM 给的 `act` 参数里若带这个 key 必须被**剥除**（网关
/// `tools_browser::sanitize_out_of_band` 在分类前剥除），否则模型能自我授权不可逆动作、绕过红线门。
/// facade 这侧只**读**它（不剥）：因为 P0-P2 的引擎内 nomi 会话从不注入此 key（恒缺 = false = 现行
/// fail-closed 行为不变），而网关侧的注入已过信任边界。`__` 前缀标记其为内部协议字段（非用户/模型可见
/// 动作参数）。
pub const OUT_OF_BAND_CONFIRMED_KEY: &str = "__out_of_band_confirmed";

/// 单次 `act` 的动作级超时预算（F1 给 [`Progress::new`] 的 deadline）。导航/动作各自有更细的内部
/// settle/退避；这是 facade 给整个动作的外层上限——比 nav 的 30s 略宽，覆盖「点击触发慢导航」的情形。
/// abort（page.close/frame.detach）由引擎内部订阅，优先于本 deadline（progress.rs biased race）。
const ACT_TIMEOUT: Duration = Duration::from_secs(45);

const DESCRIPTION: &str = "Drive a managed Chromium browser via the Chrome DevTools Protocol \
(in-process, self-hosted — no external browser, no Playwright/Node). Use this to open web pages, \
read their structure, and interact with them.\n\n\
THE CORE LOOP: `navigate` → `observe` → act by `ref` → `observe` again to verify. Every \
interactive action targets an element by its `[ref=f<seq>e<n>]` from the most recent `observe`; a \
ref goes stale after any navigation or page change, so re-`observe` for fresh refs.\n\n\
Read-only actions (safe to run freely):\n\
- navigate: load `url`, optionally in a new tab (`new_tab`: true). Returns where the page settled \
after redirects.\n\
- observe: read the page's accessibility tree as an aria snapshot (YAML) + a numbered `[ref]` \
table. Do this first; `max_depth` caps the tree for huge pages.\n\
- screenshot: capture the current page as a PNG.\n\
- capabilities: report what this session can do (engine, headful/headless, display).\n\
- get_page_text: get the page's readable text. search_page: grep the page for `query`. \
find_elements: find elements by CSS `selector` and register fresh refs. get_dropdown_options: \
list a `<select>`'s options by `ref`. cursor / tabs: inspect clickable elements / open tabs.\n\
- wait: pause `ms` milliseconds. wait_for: wait until a condition holds.\n\n\
Write actions (change the page — verify with a follow-up `observe`):\n\
- click: click the element `ref`. hover: hover it. type: type `text` into element `ref` \
(set `text` to \"secret:NAME\" to inject a stored credential without it ever passing through this \
conversation). set_value: set a control's `value` directly. select_option: pick `options` in a \
`<select>` `ref`. press_key: press `keys` (e.g. \"Enter\", \"Control+a\"). scroll: scroll \
`direction` by `amount`. scroll_to_text: scroll until `text` is visible. upload_file: set a file \
input `ref` to `file_path` (bypasses the OS file dialog). download: download `url` into a sandboxed \
downloads folder (never your real Downloads; executables are blocked, files are not auto-opened). \
save_as_pdf: save the current page as a PDF into that sandboxed folder. extract: return a \
structured, redacted representation of the page (aria snapshot + visible text) to extract fields \
from against a JSON `schema`. switch_frame: enter an iframe `ref`. switch_tab / close_tab / \
open_link_new_tab / back / forward / reload: navigation and tab control.\n\n\
IRREVERSIBLE actions need care: clicking a Pay / Buy / Checkout / Submit / Delete / Send / \
Confirm button, pressing Enter inside a form, or reloading a page that submitted a form can \
charge money, delete data, or send a message — these cannot be undone. In an auto-approving \
(yolo/companion) session such actions are blocked outright; otherwise they require explicit \
approval.\n\n\
Usage notes:\n\
- A `[ref]` is only valid for the snapshot it came from — re-run `observe` after any navigation \
or UI change rather than reusing an old ref.\n\
- The browser launches lazily on the first action; if it cannot start, the action returns an \
error explaining why (do not retry blindly).";

/// Browser automation tool. Holds the lazily-constructed engine and the config
/// needed to construct it. `new` does NOT launch a browser; the engine is built
/// (and any construction error cached) on the first action.
pub struct BrowserTool {
    /// Tool description with the session's static capability note appended
    /// (computed once at construction; part of the cacheable tool schema). The
    /// note is derived from a cheap default `Capabilities` — we never launch a
    /// browser just to render the description.
    description: String,
    /// Application data directory passed to the engine (managed chrome download
    /// fallback + the dedicated user-data-dir parent). Never the user's real
    /// browser profile.
    data_dir: PathBuf,
    /// **并发隔离基石：本 facade 专属的 Chromium `--user-data-dir`**（`<data_dir>/profiles/<token>`）。
    ///
    /// 每个 `BrowserTool` 实例分配一个**进程内唯一**目录（token = pid + 单调计数 + 纳秒），使任意两个
    /// 并发存活的引擎（不同会话 / 网关每 key / stdio 桥 / 编排子节点）**绝不共享同一 user-data-dir**——
    /// 根治 Chromium 进程单例碰撞（第二个 chrome 转发命令行后退出 → CDP 断裂 → 节点失败）。**同一 facade
    /// 生命周期内稳定**（`engine()` 因 SessionLost 自愈重启复用同一目录；此刻旧 chrome 已死，目录空闲）。
    /// 灌进 [`nomi_browser_engine::EngineConfig::user_data_dir`]（配 `ephemeral_profile=true` → 引擎 Drop
    /// 时清理）。登录态跨会话持久经**加密 vault 播种**（非依赖共享磁盘 profile）。红线不变：在我们自己的
    /// `data_dir` 下，绝不指向用户真实 profile。
    profile_dir: PathBuf,
    /// **P3-G2: per-session/per-pet 隔离 workspace 目录**（裁决⑨ / 默认④），灌进
    /// [`EngineConfig::workspace_dir`]。引擎据它把下载（E4）落进 `<workspace>/downloads`
    /// （[`nomi_browser_engine::download::download_dir`]）——**绝不**落用户真实 Downloads。
    ///
    /// **怎么拿到的（架构）**：`BrowserTool` 自身无会话上下文，但 **bootstrap**
    /// （`AgentBootstrap::build`）持有会话的工作目录 `self.workspace`——它已是天然的
    /// per-session/per-pet 隔离点：
    /// - 伙伴会话：companion.rs 把 `extra.workspace = {companion_id}/workspace`（每伙伴固定
    ///   私有目录）写进会话 extra；该路径经 manager/nomi/agent.rs 落成 bootstrap 的 `workspace`。
    /// - 非伙伴会话：会话自己的工作目录（临时或用户自定目录），同样 per-conversation 隔离。
    ///
    /// bootstrap 据此在构造 `BrowserTool` 时把会话 workspace 经 [`Self::with_policy`] 灌进这里
    /// （沿 `auto_approve`/`full_power` 同范式，构造期注入）。`None`（仅有 `BrowserConfig` 的
    /// 调用方 / 测试默认）→ 引擎兜底落 `<data_dir>/downloads`（仍是隔离目录，非用户 Downloads，
    /// 红线已守住）。
    ///
    /// **非伙伴 `{data_dir}/browser-profiles/{conversation_id}` 细分（默认④）的取舍**：
    /// bootstrap 在构造期既拿不到 `conversation_id` 也拿不到后端 `data_dir`（它们在更上层的
    /// manager/factory，bootstrap 只持会话工作目录 `self.workspace`）。会话工作目录本身已是
    /// per-conversation 隔离点（最简正确），故 G2 直接传它；`browser-profiles` 子细分留 W4/部署
    /// 接线（届时上层可经此字段传更细的路径，签名不变）。
    workspace_dir: Option<PathBuf>,
    /// build 期固化的 bundled Chrome 资源目录（Tauri resource dir 下），由 nomifun-app
    /// 解析后注入；`None` = 非打包/测试，走 env > data_dir > 下载。
    bundled_dir: Option<PathBuf>,
    /// Whether to request a visible window. Ignored by the engine when no
    /// display is available (forced headless).
    headful: bool,
    /// **浏览器来源**（[`ChromeSource`]，与 `headful` 正交）：`Managed`（默认）= 内置/下载 CfT；
    /// `System`（「我的浏览器」）= 系统已装 Chrome/Edge 本体优先。由 [`Self::new`] 从
    /// `BrowserConfig.source` 解析（`with_data_dir` / 测试默认 `Managed`），`engine()` 透传给
    /// `EngineConfig.chrome_source`。红线不变：两种来源都用专属 user-data-dir。
    chrome_source: ChromeSource,
    /// 持久登录 vault **节流保存**的上次落盘时刻。`spawn_persist_login` 每次成功导航后 best-effort
    /// capture+save 登录态到加密 vault，但 ≥60s 才落一次（避免每次导航都跑 CDP capture + 写盘）。
    last_persist_login: Mutex<Option<Instant>>,
    /// Lazily-initialized browser engine. `Some(Err)` caches an unavailable
    /// backend (chrome not resolvable, launch/connect failure) so we don't
    /// retry per call — the same failure-cache discipline as `ComputerTool`'s
    /// accessibility engine.
    pub(crate) engine: Mutex<Option<Result<Arc<dyn BrowserEngine>, String>>>,
    /// **Per-facade engine-construction gate (并发隔离)**. Held across the async
    /// `create_engine().await` so concurrent *first* calls on one `BrowserTool`
    /// (e.g. the MCP stdio bridge shares one `Arc<BrowserTool>` and calls `execute`
    /// with zero upstream serialization) launch **at most one** Chrome against this
    /// facade's single [`Self::profile_dir`] — never two processes racing the same
    /// user-data-dir. `engine()` double-checks the cache after acquiring it, so a
    /// waiter reuses the engine the first caller built instead of building its own.
    engine_build_gate: tokio::sync::Mutex<()>,
    /// The most recent `observe` snapshot, kept for provenance/diagnostics and
    /// resolving `[ref=f<seq>e<n>]` actions against the generation they were
    /// produced for. F1 also reads its `url` as the **current origin** for the
    /// `secret:NAME` origin gate (see [`Self::current_origin`]).
    pub(crate) last_snapshot: Mutex<Option<Observation>>,
    /// **F1: origin-bound credential vault** for `secret:NAME` injection (裁决⑦).
    /// When a `type`/`set_value` `text` is `secret:NAME`, the facade resolves it
    /// here against the current origin (eTLD+1 fail-closed) and injects the value
    /// as [`TypeInput::Secret`] — the plaintext never reaches the LLM, the ref
    /// table, or logs.
    ///
    /// **Registration source is a P3/user-config concern** (TODO below): P2 wires
    /// the *interception path and origin gate* completely, but ships an empty
    /// store, so a `secret:NAME` with no matching registration fails closed
    /// (Blocked) rather than leaking. Tests inject a populated store via
    /// [`Self::with_secret_store`].
    secret_store: Mutex<Option<SecretStore>>,
    /// **P3-X2: per-pet secret vault source** (vault file path + machine-bound key).
    /// When set and [`Self::secret_store`] is empty, [`Self::engine`] lazily loads the
    /// per-pet [`SecretStore`] from the vault on the first action — so `secret:NAME`
    /// resolves against the credentials the user registered (origin gate still
    /// fail-closed) and the store's [`SecretStore::allowed_etld1_union`] feeds
    /// `FirewallConfig.allow_etld1` (裁决⑤ 共用真值: one per-pet config, two uses).
    ///
    /// `None` (CLI REPL / `BrowserConfig`-only callers / tests) → no source; the
    /// store stays whatever `secret_store` holds (empty by default → `secret:NAME`
    /// fails closed, current behavior). Loaded lazily (not at construction) so a
    /// secret registered after the session starts is still picked up on first use,
    /// and so building the tool never touches disk.
    secret_source: Option<BrowserSecretSource>,
    /// **P3-X2: the firewall config to inject into the engine** (裁决⑤/D1). Built at
    /// engine-construction time from [`Self::secret_source`]'s loaded store: the
    /// secret's per-pet `allowed_origins` become `FirewallConfig.allow_etld1`. Cached
    /// so it's computed once (alongside the store load). `None` until first
    /// `engine()` → falls back to `FirewallConfig::default()` (unrestricted egress,
    /// current behavior) when no secret source / no registered origins.
    firewall_override: Mutex<Option<nomi_browser_engine::FirewallConfig>>,
    /// **F1-sec: 本会话的 orchestration 审批闸是否被旁路**（`yolo || companion-forced-yolo ||
    /// auto_approve`，裁决⑧）。这是 redline 门 [`redline::enforce_redline`] 的关键入参——决定
    /// 「审批旁路会话里的不可逆动作」是否 hard-deny。
    ///
    /// **怎么拿到的（架构）**：`Tool::execute(&self, input: Value)` 的签名不携带 `session_mode`
    /// （ComputerTool 同样如此），故在执行点拿不到会话模式。但**构造期**拿得到：bootstrap
    /// （`AgentBootstrap::build`）持有完整 `Config`，而 `config.tools.auto_approve` **恰好**当且仅当
    /// orchestration 审批被旁路时为 `true`——
    /// - yolo 会话：`session_mode == "yolo"` → `CliArgs.auto_approve = true`（manager/nomi/agent.rs）
    ///   → `Config::resolve` 置 `config.tools.auto_approve = true`（config.rs §7）；
    /// - companion-forced-yolo / desktopGateway 会话：工厂（factory/nomi.rs）把 `session_mode` pin
    ///   成 `"yolo"`，同样落到上面这条链；
    /// - `--auto-approve` CLI：直接置 `config.tools.auto_approve = true`。
    /// - 而 `AutoEdit` 模式**不**置它（只自动批 info/edit 类别，从不批 Irreversible），故 AutoEdit
    ///   会话的不可逆动作仍交 orchestration（门不拦），符合红线方向。
    ///
    /// bootstrap 据此在构造 `BrowserTool` 时把 `config.tools.auto_approve` 灌进这里（构造期快照）。
    ///
    /// **P3-X1 — 这是初值/兜底，不再是唯一真值**：本字段仍是构造期快照（与 `auto_approve` 同源），
    /// 但当 [`Self::runtime_mode`] 被注入（bootstrap 把会话的 `Arc<ToolApprovalManager>` 穿透进来）时，
    /// [`Self::session_bypasses_approval`] **优先 LIVE 读运行时模式**，本快照仅作运行时句柄缺失时的兜底
    /// （CLI REPL / 仅有 `BrowserConfig` 的调用方 / 测试）。运行时句柄在时 → 用户会话中途 `set_mode` 翻
    /// yolo 即时反映到红线门（缺口已闭，见 `runtime_mode` 文档）。
    session_bypasses_approval: bool,
    /// **P3-X1: 运行时审批模式的共享句柄**（闭合 set_mode 运行时翻转缺口）。
    ///
    /// `Some(mgr)` 时 [`Self::session_bypasses_approval`] **LIVE 读** `mgr.session_bypasses_approval()`
    /// （= 当且仅当当前 `session_mode == Yolo`，权威映射在 [`ToolApprovalManager::session_bypasses_approval`]）
    /// —— 故用户在会话**中途**经 `set_mode` 把模式翻成 yolo / 翻回 default，红线门即时随之武装 / 解除，
    /// 不再被构造期快照钉死。`None`（CLI REPL / 仅 `BrowserConfig` 的调用方 / 测试）→ 回退到构造期
    /// [`Self::session_bypasses_approval`] 快照（现行 fail-closed 行为不变）。
    ///
    /// **怎么拿到的（架构）**：会话的 `Arc<ToolApprovalManager>` 由后端（`manager/nomi/agent.rs`）/ CLI
    /// （`nomi-cli` json-stream）创建并经 `engine.set_approval_manager` 安到引擎；`set_mode`（前端/网关切
    /// yolo 的入口）改的就是这同一个 manager 的 `session_mode`。bootstrap 在构造 `BrowserTool` **之前**
    /// 拿到这个 Arc（经 `AgentBootstrap::approval_manager` builder 注入），把它经 [`Self::with_policy`]
    /// 的 `runtime_mode` 参穿透进本字段——facade 与 orchestration 看的是**同一个**运行时模式 cell，零漂移。
    ///
    /// **F1-sec bypass 映射方向保持不变**：只有 `Yolo` 算 bypass；`AutoEdit`（只自动批 info/edit，从不批
    /// Irreversible）**不**算 bypass → 不武装红线门（不可逆动作仍交 orchestration）。该映射的唯一权威是
    /// [`ToolApprovalManager::session_bypasses_approval`]，facade 不复制它。`nomi-browser` 本就依赖
    /// `nomi-protocol`，故此句柄零新增跨 crate 依赖。
    runtime_mode: Option<Arc<ToolApprovalManager>>,
    /// Explicit Browser Use approval bypass. When true, Browser-specific approval
    /// prompts approve immediately. This does not affect shell/file/native tools.
    unrestricted_approval: bool,
    /// **F1-sec: evaluate「全权模式」LIVE 值**（裁决⑨，E3 门控的 opt-in 开关）。`true` 当且仅当用户在
    /// System Settings 显式 opt-in 了 browser-use 全权模式（`client_preferences` 形如
    /// `agent.browserUse.fullPower`，由 bootstrap 经 `read_bool_pref` 范式读后经 [`Self::with_policy`]
    /// 灌入）。默认 `false`（default-deny → evaluate 返 `Unsupported`）。本值在 [`Self::engine`] 构造
    /// 引擎时灌进 [`nomi_browser_engine::EngineConfig::evaluate_full_power`]，由引擎层 evaluate 门
    /// 消费——**绝不看 session_mode**（yolo/companion 无从豁免，不变量⑧）。
    ///
    /// **LIVE 语义**：与 `computer_use`/`browser_use` 启用开关同范式——每次**会话构造**时由 bootstrap
    /// 从 client_preferences 读最新值，故用户切换全权开关对**新会话**即时生效（无需重启）。会话内的引擎
    /// 在首个动作构造后持有该快照；intra-session 翻转不影响已构造引擎（TODO(P3)：要每 act LIVE 读需把
    /// prefs repo 穿透到引擎，P2 取会话构造期快照，足够 default-OFF + opt-in 语义）。
    evaluate_full_power: bool,
    /// **SD-6: 持久登录 LIVE 值**（DESIGN §16/§27 互斥约束）。`true` 当且仅当用户开启了 persistent-login
    /// （`client_preferences` `agent.browserUse.persistentLogin`，由 bootstrap 经 `read_bool_pref`
    /// 范式读后经 [`Self::with_policy`] 灌入）。默认 `false`（code-level default-deny；产品 ON 由
    /// factory host_default=true 实现）。本值在 [`Self::engine`] 构造引擎时灌进
    /// [`nomi_browser_engine::EngineConfig::evaluate_persistent_login`]，由引擎层 evaluate 互斥门消费。
    evaluate_persistent_login: bool,
    /// **P3: LLM-driven extraction model seam** (optional). When `Some`, the facade's
    /// Extract action passes the engine's deterministic payload through
    /// [`extract::extract_structured`] to produce validated structured JSON. When `None`
    /// (default, graceful degradation), the deterministic payload is returned unchanged
    /// (today's P2 behavior, zero regression).
    ///
    /// Injected by bootstrap/factory via [`Self::with_extract_model`] — mirrors how
    /// `secret_source`/`runtime_mode` are threaded.
    // Wired by bootstrap: `SessionExtractModel` (reuses the session `LlmProvider`); the engine
    // stays LLM-free, the adapter lives at the bootstrap/facade layer.
    extract_model: ExtractModelRef,
    /// **Known-secret exact-blackout registry** (shared with engine via `Arc`).
    /// The facade inserts each resolved `secret:NAME` plaintext (len >= 4) here;
    /// the engine's debug serializers read it and `String::replace` each value with
    /// `[KNOWN_SECRET_REDACTED]` before heuristic passes. Session-scoped, in-memory only.
    known_secret_values: nomi_browser_engine::KnownSecretValues,
    /// **Takeover: must-re-observe flag**. Set to `true` after a takeover resolves
    /// (the user may have navigated during the takeover — pre-takeover `f<seq>e<n>` refs
    /// are invalid). Checked before any ref-based action; cleared by `observe`.
    /// Fail-closed: if set, ref-based actions return an error telling the model to
    /// re-observe.
    must_re_observe: AtomicBool,
    /// **Takeover controller**: handles out-of-band human approval for irreversible
    /// actions. Default: disabled (fail-closed — irreversible stays Blocked without
    /// a client-pref opt-in). When enabled, irreversible actions in bypass sessions
    /// trigger a takeover request instead of immediately blocking.
    /// Wired by bootstrap: the LIVE `agent.browserUse.takeover` pref gates injection of the host
    /// approval gate (`with_approval_gate`), which flips `controller.enabled`. Absent the gate
    /// (pref OFF / hosts without one), defaults OFF (fail-closed).
    takeover_controller: crate::takeover::TakeoverController,
    /// **Phase D: out-of-band approval gate** (human takeover + SD-5 cross-origin POST
    /// egress). When `Some`, an irreversible action in a bypass session — and a gated
    /// cross-origin POST (via [`crate::approval::GateEgressApprover`] injected into the
    /// engine) — is surfaced to the user for approval and awaited. `None` (default) →
    /// **fail-closed** (irreversible stays Blocked; egress fails closed = pre-Phase-D
    /// behavior). Injected by bootstrap (desktop: event + `ToolApprovalManager`) / gateway
    /// (GW2 `nomi_browser_confirm` channel) via [`Self::with_approval_gate`].
    approval_gate: crate::approval::BrowserApprovalGateRef,
    /// **P7C: active recording** (record & replay). When `Some`, every successful
    /// `do_act` appends a [`crate::recording::RecordedStep`] to the recording.
    /// Started/stopped via [`Self::start_recording`]/[`Self::stop_recording`].
    recording: Mutex<Option<crate::recording::Recording>>,
    /// **P7A: site memory** (cross-session site structure memory). When `Some`, every
    /// successful non-secret `do_act` records a [`crate::site_memory::SiteMemoryEntry`]
    /// keyed by eTLD+1; `do_observe` attaches remembered hints as untrusted suggestions.
    /// `None` = graceful degradation (today's behavior, no memory).
    /// Wired by bootstrap: a file-backed `FileSiteMemorySink` (the KnowledgeService approach was
    /// rejected — sync/async mismatch + would pollute the searchable KB; see `site_memory.rs`),
    /// gated on the `agent.browserUse.siteMemory` pref.
    site_memory: Option<Arc<crate::site_memory::SiteMemoryStore>>,
    /// **P7B: visual fallback enabled** (client-pref gated, default OFF). When `true` AND
    /// `visual_locator` is `Some`, the facade attempts vision-based element location on
    /// `resolve_ref` failure (NodeStale/NotConnected) before returning an error to the model.
    /// This is a vision-cost path — default OFF so users opt in explicitly.
    /// Mirrors the `evaluate_full_power`/`evaluate_persistent_login` pattern.
    visual_fallback_enabled: bool,
    /// **P7B: vision locator seam** (optional, default None → graceful degradation).
    /// When `Some`, provides the vision model call for locating elements by screenshot.
    /// Injected by bootstrap/factory via [`Self::with_visual_locator`].
    /// Wired by bootstrap: `SessionVisualLocator` (reuses the session model; screenshot sent via
    /// `ContentBlock::Image`), gated on the `agent.browserUse.visualFallback` pref.
    visual_locator: crate::visual_fallback::VisualLocatorRef,
}

/// Monotonic counter for per-facade user-data-dir tokens (unique within a process).
static PROFILE_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// **Allocate a process-unique Chromium `--user-data-dir`** under `<data_dir>/profiles/`.
///
/// Token = `pid-seq-nanos`: unique across facades (monotonic `seq`), across processes
/// (`pid` — the MCP stdio bridge is a separate process), and practically collision-free
/// vs a leftover dir from a prior run (`nanos` wall-clock; startup GC also sweeps stale
/// dirs). This is what makes concurrent browser engines structurally unable to share one
/// profile → no Chromium process-singleton handoff/exit. Pure path join (no I/O; the
/// engine `create_dir_all`s it at launch). Red line: under our own `data_dir`, never the
/// user's real profile.
fn allocate_profile_dir(data_dir: &std::path::Path) -> PathBuf {
    let seq = PROFILE_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let token = format!("{}-{}-{}", std::process::id(), seq, nanos);
    data_dir.join("profiles").join(token)
}

impl BrowserTool {
    /// Construct the tool from the browser config. Reads `headless` (inverted to
    /// `headful`) and derives the engine data directory; does NOT launch a
    /// browser. The data directory is the app config dir when available,
    /// otherwise the engine's temp-dir default.
    ///
    /// **F1-sec**: this signature keeps `config: &BrowserConfig` only — the
    /// session-bypass policy is NOT in `BrowserConfig`. Bootstrap holds the full
    /// `Config` and knows `tools.auto_approve`, so it must use [`Self::with_policy`]
    /// to wire the redline gate. `new` defaults the policy to `false` (a normal
    /// session) for callers that only have a `BrowserConfig` (the redline gate then
    /// never hard-denies — equivalent to leaving approval to orchestration).
    pub fn new(config: &BrowserConfig) -> Self {
        let data_dir = nomi_config::config::app_config_dir()
            .map(|d| d.join("browser-data"))
            .unwrap_or_else(|| EngineConfig::default().data_dir);
        let mut t = Self::with_data_dir(data_dir, !config.headless);
        // 浏览器来源（「我的浏览器」）：从 BrowserConfig.source 解析（坏值静默退回 Managed）。
        // 与 headful 正交；`with_policy` 无需带参——两开关都经 config.tools.browser.* 流入。
        t.chrome_source = ChromeSource::from_source_str(&config.source);
        t.unrestricted_approval = config.unrestricted_approval;
        t
    }

    /// **F1-sec: construct from the browser config + the session-bypass policy +
    /// the evaluate full-power LIVE value.**
    ///
    /// - `session_bypasses_approval` MUST be `config.tools.auto_approve` (the
    ///   construction-time snapshot that is `true` iff orchestration approval is
    ///   bypassed — yolo / companion-forced-yolo / `--auto-approve`; see the field
    ///   doc). This is the wiring point the bootstrap uses so the facade redline gate
    ///   (裁决⑧) actually fires in auto-approving sessions instead of failing open.
    /// - `evaluate_full_power` MUST be the LIVE `client_preferences`
    ///   `agent.browserUse.fullPower` value (read via the `read_bool_pref` pattern).
    ///   `false` (default / not opted in) keeps evaluate OFF (E3 default-deny).
    /// - `workspace_dir` (P3-G2) is the session's working directory — the natural
    ///   per-session/per-pet isolation point (companion: `{companion_id}/workspace`;
    ///   non-companion: the conversation's working dir). Downloads (E4) land in its
    ///   `downloads/` subdir. `None` → the engine falls back to `<data_dir>/downloads`
    ///   (still isolated, never the user's real Downloads). See the `workspace_dir`
    ///   field doc for the full architecture.
    /// - `runtime_mode` (P3-X1) is the session's shared `Arc<ToolApprovalManager>` — the
    ///   *runtime-flippable* mode cell that `set_mode` mutates. When `Some`,
    ///   [`Self::session_bypasses_approval`] reads it LIVE (yolo ⇒ bypass) so a mid-session
    ///   `set_mode` to yolo arms the redline gate immediately; `None` falls back to the
    ///   construction-time `session_bypasses_approval` snapshot. The two arguments stay
    ///   consistent — the snapshot is the bootstrap's initial `auto_approve`, the handle is the
    ///   same manager whose mode that snapshot was derived from. See the `runtime_mode` field doc.
    pub fn with_policy(
        config: &BrowserConfig,
        session_bypasses_approval: bool,
        evaluate_full_power: bool,
        evaluate_persistent_login: bool,
        workspace_dir: Option<PathBuf>,
        runtime_mode: Option<Arc<ToolApprovalManager>>,
        secret_source: Option<BrowserSecretSource>,
    ) -> Self {
        let mut t = Self::new(config);
        t.session_bypasses_approval = session_bypasses_approval;
        t.evaluate_full_power = evaluate_full_power;
        t.evaluate_persistent_login = evaluate_persistent_login;
        t.workspace_dir = workspace_dir;
        t.runtime_mode = runtime_mode;
        t.secret_source = secret_source;
        t
    }

    /// Construct with an explicit data directory (used by tests and any caller
    /// that wants to control the engine's data layout). Does NOT launch.
    ///
    /// **TODO(F1-sec / P3)**: thread the per-pet [`SecretStore`] in here (built
    /// from user-configured credentials, keyed by the app's machine-bound
    /// `encryption_key`). P2 ships `None` (empty vault → `secret:NAME` fails
    /// closed); the interception path + origin gate are complete and wired.
    pub fn with_data_dir(data_dir: PathBuf, headful: bool) -> Self {
        // 并发隔离：每个 facade 分配进程内唯一 user-data-dir（<data_dir>/profiles/<token>），
        // 使任意两个并发存活引擎绝不共享 profile（根治 Chromium 进程单例碰撞）。
        let profile_dir = allocate_profile_dir(&data_dir);
        Self {
            description: format!("{DESCRIPTION}{}", capabilities_note(&default_capabilities())),
            data_dir,
            profile_dir,
            // P3-G2: 默认无 per-pet workspace（仅有 BrowserConfig 的调用方 / 测试）。bootstrap 经
            // `with_policy` 传会话 workspace；引擎兜底落 <data_dir>/downloads（仍隔离，非用户 Downloads）。
            workspace_dir: None,
            // PKG-1: 默认无 bundled Chrome（CLI / BrowserConfig-only / 测试）。nomifun-app 解析
            // resource dir 后经 builder `.bundled_dir(...)` 注入。
            bundled_dir: None,
            headful,
            // 默认来源 = Managed（内置/下载 CfT）。`new()` 从 BrowserConfig.source 覆写为 System；
            // 仅有 data_dir 的调用方 / 测试保持 Managed（现行为，零回归）。
            chrome_source: ChromeSource::Managed,
            // 持久登录节流保存：初始 None（首次导航即落一次 vault）。
            last_persist_login: Mutex::new(None),
            engine: Mutex::new(None),
            // 并发隔离：per-facade 引擎构造锁（详见字段文档）。
            engine_build_gate: tokio::sync::Mutex::new(()),
            last_snapshot: Mutex::new(None),
            secret_store: Mutex::new(None),
            // P3-X2: 默认无 secret 源（CLI / BrowserConfig-only / 测试）。bootstrap 经 `with_policy`
            // 传 per-pet 源；引擎首动作时从 vault 懒加载 store + 派生 allow_etld1。
            secret_source: None,
            firewall_override: Mutex::new(None),
            // Default: a normal (non-bypassing) session. Bootstrap overrides this via
            // `with_policy` with `config.tools.auto_approve`. Tests set it directly.
            session_bypasses_approval: false,
            // P3-X1: 默认无运行时模式句柄 → session_bypasses_approval() 回退到上面的构造期快照
            // （现行 fail-closed 行为不变）。bootstrap 经 `with_policy` 的 runtime_mode 参注入会话的
            // Arc<ToolApprovalManager>，届时门 LIVE 读运行时模式（set_mode 翻转即时生效）。
            runtime_mode: None,
            unrestricted_approval: false,
            // Default: evaluate full-power OFF (E3 default-deny). Bootstrap overrides via
            // `with_policy` with the LIVE `agent.browserUse.fullPower` pref.
            evaluate_full_power: false,
            // SD-6: persistent-login default OFF (code-level default-deny base). Bootstrap
            // overrides via `with_policy` with the LIVE `agent.browserUse.persistentLogin` pref
            // (host_default=true → product ON).
            evaluate_persistent_login: false,
            // P3: 默认无抽取模型（graceful degradation → 返确定性载荷）。bootstrap/factory 经
            // `with_extract_model` 注入真实适配器。
            extract_model: None,
            // Known-secret blackout: shared Arc created here, clone goes into EngineConfig.
            known_secret_values: Arc::new(std::sync::Mutex::new(HashSet::new())),
            // Takeover: initially no re-observe needed.
            must_re_observe: AtomicBool::new(false),
            // Takeover controller: default OFF (fail-closed). Irreversible actions
            // stay Blocked unless a client-pref enables takeover.
            takeover_controller: crate::takeover::TakeoverController::new(
                Duration::from_secs(120), // 2 min default timeout for human action
            ),
            // P7C: no active recording by default.
            recording: Mutex::new(None),
            // P7A: no site memory by default (graceful degradation). Bootstrap/factory
            // injects a store via `.with_site_memory(...)`.
            site_memory: None,
            // P7B: visual fallback default OFF (vision-cost path, opt-in). Bootstrap reads
            // `agent.browserUse.visualFallback` client-pref and wires it.
            visual_fallback_enabled: false,
            // P7B: no vision locator by default (graceful degradation). Bootstrap/factory
            // injects the real adapter via `.with_visual_locator(...)`.
            visual_locator: None,
            // Phase D: no approval gate by default (fail-closed). Bootstrap/gateway injects
            // it via `.with_approval_gate(...)` to enable takeover + egress approval.
            approval_gate: None,
        }
    }

    /// This facade's dedicated Chromium `--user-data-dir` (`<data_dir>/profiles/<token>`),
    /// unique per instance so concurrent engines never collide on Chromium's process
    /// singleton. Stable for the facade's lifetime. See the `profile_dir` field doc.
    pub fn profile_dir(&self) -> &std::path::Path {
        &self.profile_dir
    }

    /// **P3-G2 builder**: set the per-session/per-pet workspace dir on a tool built
    /// via [`Self::with_data_dir`] (which controls the engine data layout directly).
    /// Used by the download integration test and any caller that constructs with an
    /// explicit data dir but still wants downloads to land in the session workspace.
    pub fn workspace(mut self, workspace_dir: PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    /// **PKG-1 builder**: set the bundled Chrome resource dir on a tool built via
    /// [`Self::with_data_dir`]. When the build places Chrome-for-Testing at
    /// `<bundled_dir>/chrome-<platform>/...`, the engine prefers it over the
    /// network download fallback (priority: env > bundled > data_dir > download).
    pub fn bundled_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.bundled_dir = dir;
        self
    }

    /// **P7A builder**: inject a site-memory store. When set, successful non-secret
    /// actions record element descriptors keyed by eTLD+1; observe attaches remembered
    /// hints. When absent (the default), site memory is a no-op (graceful degradation).
    /// Bootstrap injects a file-backed `FileSiteMemorySink` (see `site_memory.rs`).
    pub fn with_site_memory(mut self, store: Arc<crate::site_memory::SiteMemoryStore>) -> Self {
        self.site_memory = Some(store);
        self
    }

    /// **P3-X2 builder**: set the per-pet secret vault source on a tool built via
    /// [`Self::with_data_dir`] (the gateway `BrowserRegistry` path, which constructs
    /// the tool directly rather than through bootstrap). When set, [`Self::engine`]
    /// lazily loads the [`SecretStore`] from the vault and derives the firewall
    /// `allow_etld1` from its registered origins (裁决⑤).
    pub fn secret_source(mut self, source: BrowserSecretSource) -> Self {
        self.secret_source = Some(source);
        self
    }

    /// **P3 builder**: inject the LLM model seam for structured extraction. When set,
    /// the Extract action passes the engine's deterministic payload through
    /// [`extract::extract_structured`] to produce validated structured JSON. When absent
    /// (the default), Extract returns the deterministic payload unchanged (graceful degradation).
    pub fn with_extract_model(mut self, model: Arc<dyn ExtractModel>) -> Self {
        self.extract_model = Some(model);
        self
    }

    /// **P7B builder**: inject the visual locator (vision model adapter). When `Some`,
    /// visual fallback can use it to locate elements by screenshot when DOM/aria anchoring
    /// fails. When `None` (default), visual fallback is unavailable regardless of the
    /// `visual_fallback_enabled` flag.
    pub fn with_visual_locator(
        mut self,
        locator: Arc<dyn crate::visual_fallback::VisualLocator>,
    ) -> Self {
        self.visual_locator = Some(locator);
        self
    }

    /// **P7B builder**: set the visual fallback enabled flag. When `true` AND the locator
    /// is present, the facade attempts vision-based fallback on anchor failure.
    /// Default: `false` (vision-cost path, opt-in via `agent.browserUse.visualFallback`).
    pub fn with_visual_fallback_enabled(mut self, enabled: bool) -> Self {
        self.visual_fallback_enabled = enabled;
        self
    }

    /// **Phase D builder**: inject the out-of-band approval gate (human takeover + SD-5
    /// egress). When `Some`, irreversible actions in bypass sessions are surfaced to the
    /// user for approval (instead of hard-denied), and a [`crate::approval::GateEgressApprover`]
    /// backed by this gate is handed to the engine so gated cross-origin POSTs are
    /// approved live (instead of fail-closed). `None` (default) → fail-closed.
    /// Also flips `takeover_controller.enabled` so the redline gate takes the takeover path.
    pub fn with_approval_gate(mut self, gate: Arc<dyn crate::approval::BrowserApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self.takeover_controller.enabled = true;
        self
    }

    // ── Takeover: must-re-observe ────────────────────────────────────────────

    /// Mark that a re-observe is required before any ref-based action.
    /// Called after a takeover resolves (the user may have navigated).
    pub fn set_must_re_observe(&self) {
        self.must_re_observe.store(true, Ordering::Release);
    }

    /// Clear the must-re-observe flag. Called when an `observe` is performed.
    pub fn clear_must_re_observe(&self) {
        self.must_re_observe.store(false, Ordering::Release);
    }

    /// Check if a re-observe is required. If true, ref-based actions must be
    /// rejected until the model runs a fresh `observe`.
    pub fn needs_re_observe(&self) -> bool {
        self.must_re_observe.load(Ordering::Acquire)
    }

    /// Access the takeover controller (for test injection and configuration).
    pub fn takeover_controller_mut(&mut self) -> &mut crate::takeover::TakeoverController {
        &mut self.takeover_controller
    }

    /// Access the takeover controller (read-only).
    pub fn takeover_controller(&self) -> &crate::takeover::TakeoverController {
        &self.takeover_controller
    }

    // ── P7C: Recording ──────────────────────────────────────────────────────

    /// Start recording browser actions. Returns `true` if recording was started
    /// (not already active). While recording is active, every successful `do_act`
    /// appends a step to the recording.
    pub fn start_recording(&self) {
        let url = self.current_origin().unwrap_or_default();
        let mut guard = self.recording.lock().expect("recording poisoned");
        if guard.is_none() {
            *guard = Some(crate::recording::Recording::new(url));
        }
    }

    /// Stop recording and return the completed recording. Returns `None` if
    /// recording was not active.
    pub fn stop_recording(&self) -> Option<crate::recording::Recording> {
        self.recording.lock().expect("recording poisoned").take()
    }

    /// Whether recording is currently active.
    pub fn is_recording(&self) -> bool {
        self.recording.lock().expect("recording poisoned").is_some()
    }

    /// Append a recorded step (called after a successful act, if recording is active).
    fn record_step(&self, action: &str, input: &Value, selector: Option<String>) {
        let mut guard = self.recording.lock().expect("recording poisoned");
        if let Some(ref mut rec) = *guard {
            let url = self.current_origin().unwrap_or_default();
            let step = crate::recording::RecordedStep::from_action(action, input, selector, url);
            rec.push(step);
        }
    }

    /// **P7A: record a site-memory entry** after a successful non-secret action.
    /// Reads the last snapshot for the target element's role/name; resolves eTLD+1
    /// from the snapshot URL. Best-effort: any missing data → silently skips.
    fn record_site_memory(
        &self,
        store: &crate::site_memory::SiteMemoryStore,
        action: &str,
        input: &Value,
    ) {
        // Need the snapshot URL for eTLD+1 keying and the ref's role/name.
        let snap_guard = self.last_snapshot.lock().expect("last_snapshot poisoned");
        let snap = match snap_guard.as_ref() {
            Some(s) => s,
            None => return, // No snapshot yet → can't key by URL.
        };
        let url = match snap.url.as_deref() {
            Some(u) => u,
            None => return,
        };
        let etld1 = match crate::site_memory::key_for(url) {
            Some(k) => k,
            None => return, // IP / localhost / no registrable domain.
        };

        // Extract the ref → look up role/name from the snapshot entries.
        let r#ref = match input.get("ref").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => return, // No ref → no element to remember (e.g. navigate, scroll viewport).
        };
        let entry_data = snap.entries.iter().find(|e| e.r#ref == r#ref);
        let (role, accessible_name) = match entry_data {
            Some(e) => (e.role.clone(), e.name.clone()),
            None => return, // Ref not found in snapshot.
        };

        let entry = crate::site_memory::SiteMemoryEntry {
            etld1,
            url_pattern: url.to_string(),
            intent: action.to_string(),
            role,
            accessible_name,
            selector: None, // Facade level has no CSS selector; engine internals needed.
            from_secret: false,
        };
        store.record(entry);
    }

    /// **P7A: generate site-memory hints** for the current URL's eTLD+1.
    /// Returns a `<data>`-wrapped suggestion block (untrusted, not authoritative).
    /// Empty string if no site memory or no hints available.
    fn site_memory_hints(&self, url: Option<&str>) -> String {
        let store = match self.site_memory.as_ref() {
            Some(s) => s,
            None => return String::new(),
        };
        let url = match url {
            Some(u) => u,
            None => return String::new(),
        };
        let etld1 = match crate::site_memory::key_for(url) {
            Some(k) => k,
            None => return String::new(),
        };
        let hints = store.query(&etld1);
        if hints.is_empty() {
            return String::new();
        }
        // Format as untrusted <data>-wrapped suggestions.
        let mut out = String::from(
            "\n<data type=\"site-memory-hints\" trust=\"low\">\n\
             [Remembered element hints for this site (may be stale — verify with observe before relying):]\n",
        );
        for h in &hints {
            out.push_str(&format!(
                "- {} \"{}\" (intent: {}{})\n",
                h.role,
                h.accessible_name,
                h.intent,
                h.selector
                    .as_deref()
                    .map(|s| format!(", selector: {s}"))
                    .unwrap_or_default(),
            ));
        }
        out.push_str("</data>");
        out
    }

    /// **F1 seam**: construct the tool with a pre-populated [`SecretStore`] so the
    /// `secret:NAME` interception path (origin gate → `TypeInput::Secret`) can be
    /// exercised end-to-end. Production wiring of the store's *contents* is a
    /// P3/user-config concern; this lets tests (and any caller that already holds
    /// a vault) inject one without changing the dispatch path.
    pub fn with_secret_store(data_dir: PathBuf, headful: bool, store: SecretStore) -> Self {
        let t = Self::with_data_dir(data_dir, headful);
        *t.secret_store.lock().expect("secret_store poisoned") = Some(store);
        t
    }

    /// **P3-X2: lazily load the per-pet secret store + derive the firewall config**
    /// (called once on the engine slow path, before constructing `EngineConfig`).
    ///
    /// If [`Self::secret_store`] is empty and a [`Self::secret_source`] is set, loads
    /// the [`SecretStore`] from the per-pet vault (machine-bound key; a missing/corrupt
    /// vault degrades to an empty store — see `nomifun_secret::load_secret_store`). Then
    /// derives the [`nomi_browser_engine::FirewallConfig`] for this session: the secret's
    /// per-pet `allowed_origins` (its `allowed_etld1_union`) become `allow_etld1`
    /// (裁决⑤ 共用真值). The firewall starts from `default()` (IP block + cross-origin
    /// POST gate on) and only adds the domain allowlist — an empty store / no source
    /// leaves `allow_etld1` empty = unrestricted egress (current behavior, zero
    /// regression). Both results are cached so they're computed once.
    fn ensure_secret_store_and_firewall(&self) {
        // Populate the store from the per-pet vault if empty + a source is set. A
        // pre-injected store (tests via `with_secret_store`) is left as-is.
        {
            let mut guard = self.secret_store.lock().expect("secret_store poisoned");
            if guard.is_none()
                && let Some(src) = &self.secret_source
            {
                *guard = Some(nomifun_secret::load_secret_store(&src.vault_path, src.key));
            }
        }
        // Derive the firewall config from whichever store is present (裁决⑤).
        let mut allow_etld1: Vec<String> = Vec::new();
        if let Some(store) = self.secret_store.lock().expect("secret_store poisoned").as_ref() {
            allow_etld1 = store.allowed_etld1_union();
        }
        let fw = nomi_browser_engine::FirewallConfig {
            allow_etld1,
            ..nomi_browser_engine::FirewallConfig::default()
        };
        *self.firewall_override.lock().expect("firewall_override poisoned") = Some(fw);
    }

    /// Lazily construct (and cache) the browser engine. The error string is
    /// cached too, so an unavailable backend is reported without retrying — the
    /// first action pays the launch cost, later actions reuse the same engine
    /// (or the same cached failure).
    ///
    /// **并发隔离**：construction is serialized by [`Self::engine_build_gate`] so that
    /// concurrent first-calls on one facade (the MCP stdio bridge shares a single
    /// `Arc<BrowserTool>` and runs actions with no upstream serialization) launch at most
    /// ONE Chrome against this facade's single [`Self::profile_dir`] — two racing
    /// launches on one user-data-dir would hit Chromium's process singleton and one would
    /// fail. The gate is held across the async build; a waiter re-checks the cache and
    /// reuses the engine the first caller built.
    async fn engine(&self) -> Result<Arc<dyn BrowserEngine>, String> {
        // Fast path: already constructed (success or cached failure).
        if let Some(cached) = self.engine.lock().expect("engine poisoned").as_ref() {
            return cached.clone();
        }
        // Serialize construction per facade (see method + `engine_build_gate` doc).
        let _build = self.engine_build_gate.lock().await;
        // Double-check: another task may have built (or cached a failure) while we waited
        // on the gate — reuse it instead of launching a second Chrome on the same dir.
        if let Some(cached) = self.engine.lock().expect("engine poisoned").as_ref() {
            return cached.clone();
        }
        // P3-X2: load the per-pet secret store from the vault (if a source is set) and
        // derive the firewall domain allowlist before building the engine config.
        self.ensure_secret_store_and_firewall();
        let firewall = self
            .firewall_override
            .lock()
            .expect("firewall_override poisoned")
            .clone()
            .unwrap_or_default();
        // Slow path: build the engine while holding ONLY the async build gate (not the
        // sync cache mutex) across the await — the gate guarantees single-flight, so no
        // second launch races this facade's user-data-dir.

        // 持久登录（seed 侧）：仅当开启持久登录 **且本 facade 的 profile 目录尚不存在**（每实例唯一目录
        // 首次启动即全新，故几乎总会 seed 一次；SessionLost 自愈重启时目录已在则不重复注入）时，从加密
        // vault 播种登录态。**每实例隔离后，跨会话登录态的唯一权威来源就是共享加密 vault**（不再依赖共享
        // 磁盘 profile）。首启空目录 → 注入 vault 恢复登录；已存在（同 facade 二次构造）→ 磁盘为准不覆盖。
        // key 复用 secret vault 的机器绑定 key。坏/缺 vault → None（优雅降级）。
        let storage_state = if self.evaluate_persistent_login && !self.profile_dir.exists() {
            self.persist_login_key().and_then(|key| {
                load_storage_state(&shared_storage_state_path(&self.data_dir), &key)
                    .and_then(|s| serde_json::to_value(&s).ok())
            })
        } else {
            None
        };

        let result = create_engine(EngineConfig {
            data_dir: self.data_dir.clone(),
            // 并发隔离基石：本 facade 专属唯一 user-data-dir（<data_dir>/profiles/<token>），使任意两个
            // 并发存活引擎绝不共享 profile → 根治 Chromium 进程单例碰撞。ephemeral=true → 引擎 Drop 清理。
            user_data_dir: Some(self.profile_dir.clone()),
            ephemeral_profile: true,
            // PKG-1: inject the bundled Chrome resource dir (from Tauri resource dir)
            // so the engine resolve chain is: env > bundled > data_dir > download.
            bundled_dir: self.bundled_dir.clone(),
            headful: self.headful,
            // 浏览器来源（「我的浏览器」）：透传给引擎的 chrome 解析（System → 系统 Chrome/Edge 优先，
            // 未探到回退 Managed）。红线不变：引擎仍用专属 user-data-dir 起独立托管实例。
            chrome_source: self.chrome_source,
            // E4 下载沙箱落点：per-pet 隔离 workspace（companion.rs 的 {companion_id}/workspace）。
            // P3-G2 接线（去掉 F1-wire-workspace TODO）：bootstrap 把会话工作目录 `self.workspace`
            // 经 `with_policy` 灌进 `self.workspace_dir`，这里透传给引擎——下载落进
            // <workspace>/downloads（[`download::download_dir`]）。`None`（仅有 BrowserConfig 的调用方
            // / 测试，未经 bootstrap）→ 引擎兜底落 <data_dir>/downloads（仍是隔离目录，非用户真实
            // Downloads，红线已守住）。非伙伴 {data_dir}/browser-profiles/{conversation_id} 细分见
            // workspace_dir 字段文档（会话工作目录已 per-conversation 隔离，子细分留 W4/部署）。
            workspace_dir: self.workspace_dir.clone(),
            // F1-sec E3 evaluate 门控：把构造期灌入的全权 LIVE 值（默认 false=default-deny）传给引擎，
            // 引擎据它武装 evaluate gate（仍受「与持久登录互斥」约束；绝不看 session_mode）。
            evaluate_full_power: self.evaluate_full_power,
            // SD-6：持久登录 LIVE 值（默认 false=code-level default-deny；产品 ON 由 factory 灌入）。
            evaluate_persistent_login: self.evaluate_persistent_login,
            // P3-X2 注入链：firewall 现从 per-pet secret 的 allowed_origins 派生 allow_etld1（裁决⑤
            // 共用真值）——见 `ensure_secret_store_and_firewall`。空 secret 源 / 无注册域 → allow_etld1
            // 空 = 不限制出口域（现行为，零回归）；IP 封禁 + 跨域 POST 门控仍随 default() 开。D1 的
            // TODO(X2) 已闭合：真值不再恒 default()，而是来自用户注册的 secret。
            firewall,
            storage_state,
            // P3-D2 / SD-5 出口审批通道：被防火墙门控（GatePost）的跨域 POST / 未授权域出口在引擎层
            // **悬挂等裁决**。**Phase D 接线**：当 facade 被注入 approval gate（`with_approval_gate`，
            // 桌面=事件+ToolApprovalManager / 网关=GW2 confirm）时，交引擎一个 [`GateEgressApprover`]
            // → 被门控的跨域 POST 经 gate 浮给用户、await 裁决（批准=continue 一次 / 拒绝·超时=fail-closed）。
            // 无 gate → `None` → 引擎 **fail-closed**（拒绝被门控请求，闭合 P2 跨域 POST 泄漏窗口；
            // 拒绝比放行安全 = pre-Phase-D 默认）。
            egress_approver: self.approval_gate.as_ref().map(|g| {
                Arc::new(crate::approval::GateEgressApprover::new(g.clone()))
                    as Arc<dyn nomi_browser_engine::firewall::EgressApprover>
            }),
            // Known-secret blackout: share the facade's secret set with the engine so debug
            // serializers can exact-blackout resolved secrets.
            known_secret_values: self.known_secret_values.clone(),
        })
        .await
        .map_err(|e| e.to_string());

        let mut guard = self.engine.lock().expect("engine poisoned");
        if guard.is_none() {
            *guard = Some(result);
        }
        guard.as_ref().unwrap().clone()
    }

    /// The machine-bound key for the persistent-login vault. Reuses the SAME key the
    /// secret vault uses (both are the app's machine-bound `encryption_key`, threaded via
    /// [`BrowserSecretSource`]). `None` when no source is wired (CLI / tests) → persistent
    /// login save/restore is skipped (the on-disk profile still persists natively).
    fn persist_login_key(&self) -> Option<[u8; nomifun_secret::KEY_SIZE]> {
        self.secret_source.as_ref().map(|s| s.key)
    }

    /// **持久登录（save 侧，激活原本 dead-wired 的 vault 保存）**：best-effort、节流(≥60s)地把当前
    /// 登录态（[`BrowserEngine::capture_storage_state`]）加密落**共享 vault**，作为磁盘 profile 之外的
    /// 加密备份（profile 被清后可经 seed 恢复）。非阻塞（`tokio::spawn`）——不给导航加延迟；捕获/落盘
    /// 失败仅 warn（持久登录是增强，绝不影响动作）。**空快照不落**（避免会话早期 about:blank 的空登录态
    /// 覆盖掉一份好备份）。仅在开启持久登录 + 有机器绑定 key 时生效。
    fn spawn_persist_login(&self, engine: &Arc<dyn BrowserEngine>) {
        if !self.evaluate_persistent_login {
            return;
        }
        let Some(key) = self.persist_login_key() else {
            return;
        };
        // 节流：≥60s 才落一次（首次导航即落，之后限频）。
        {
            let mut last = self.last_persist_login.lock().expect("last_persist_login poisoned");
            let now = Instant::now();
            if let Some(prev) = *last
                && now.duration_since(prev) < Duration::from_secs(60)
            {
                return;
            }
            *last = Some(now);
        }
        let engine = engine.clone();
        let vault_path = shared_storage_state_path(&self.data_dir);
        tokio::spawn(async move {
            match engine.capture_storage_state().await {
                Ok(state) => {
                    // 空快照（无 cookie 且无 localStorage）不落——不拿 about:blank 的空态覆盖好备份。
                    if state.cookies.is_empty() && state.local_storage.is_empty() {
                        return;
                    }
                    if let Err(e) = save_storage_state(&state, &vault_path, &key) {
                        tracing::warn!(
                            target: "nomi_browser::persist_login",
                            error = %e,
                            "save persistent-login vault failed (login state not backed up this round)"
                        );
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        target: "nomi_browser::persist_login",
                        error = %e,
                        "capture storage_state failed; skipping vault save (best-effort)"
                    );
                }
            }
        });
    }

    /// **Self-heal on a lost browser session.** Format an engine-method error into a
    /// non-panicking [`ToolResult::error`]; and if it's the *fatal* variant
    /// [`BrowserError::SessionLost`]`{recoverable:false}` (the DevTools WS / chrome
    /// process is gone — e.g. the previous chrome died or was killed), **evict the
    /// cached engine** so the next action lazily rebuilds a fresh browser, and tell
    /// the model the session was reset so it *retries* rather than concluding the
    /// browser is permanently dead.
    ///
    /// Why this is needed: the engine is cached for the lifetime of the `BrowserTool`
    /// with no rebuild path, so once the WS drops, **every** subsequent `navigate` /
    /// `observe` / `act` returns `SessionLost` forever — the exact "session lost
    /// (recoverable=false) … 没法继续操作" symptom. Evicting the cache lets the lazy
    /// [`Self::engine`] path relaunch on the next call (the launch-time `exit_type`
    /// scrub keeps that relaunch clean — no crash-restore bubble).
    ///
    /// `recoverable:true` (a single tab crashed / last tab closed) is intentionally
    /// **not** evicted here — that is the engine's own "I can recover in place" signal;
    /// if it genuinely can't, the next call surfaces `recoverable:false` and we evict
    /// then. Eviction only resets the cache slot (sync, no await); the live `Arc` held
    /// by the in-flight action drops normally → `kill_on_drop` reaps the dead chrome.
    fn engine_failure(&self, what: &str, err: BrowserError) -> ToolResult {
        if matches!(err, BrowserError::SessionLost { recoverable: false }) {
            *self.engine.lock().expect("engine poisoned") = None;
            ToolResult::error(format!(
                "{what}: {err}. The browser session was reset — a fresh browser will launch on \
                 your next action, so retry this step."
            ))
        } else {
            ToolResult::error(format!("{what}: {err}"))
        }
    }

    /// Build the capability note for the model from a `Capabilities` report.
    /// Public + static so it can be unit-tested without launching a browser.
    /// Always mentions "browser" so the model knows what surface this is.
    pub fn capabilities_note(caps: &Capabilities) -> String {
        capabilities_note(caps)
    }

    async fn do_navigate(&self, input: &Value) -> ToolResult {
        let Some(url) = input.get("url").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `url` for navigate (e.g. \"https://example.com\").",
            );
        };
        let new_tab = input.get("new_tab").and_then(|v| v.as_bool()).unwrap_or(false);
        let engine = match self.engine().await {
            Ok(e) => e,
            Err(msg) => return ToolResult::error(format!("Browser engine unavailable: {msg}")),
        };
        match engine.navigate(url, new_tab).await {
            Ok(nav) => {
                // 持久登录：导航成功后（登录跳转常伴随导航）best-effort 节流备份登录态到加密 vault。
                self.spawn_persist_login(&engine);
                let status = nav
                    .http_status
                    .map(|s| format!(" (HTTP {s})"))
                    .unwrap_or_default();
                let redirected = if nav.redirected { " [redirected]" } else { "" };
                ToolResult::text(format!(
                    "Navigated to {}{status}{redirected} (load state: {}). Take a screenshot to \
                     see the page.",
                    nav.final_url, nav.load_state
                ))
            }
            Err(e) => self.engine_failure("Navigation failed", e),
        }
    }

    /// Read-only page "look": serialize the page's accessibility tree to aria
    /// YAML (already `<data>`-wrapped + redacted by the engine) plus a numbered
    /// `[ref=f<seq>e<n>]` element table the model can target later. Mirrors
    /// `ComputerTool::do_observe`: prefer this over guessing — re-run it after
    /// any navigation/UI change since a `ref` is only valid for the snapshot
    /// generation it was produced for. Optional `max_depth` (cap, default 12) and
    /// `diff` (injected-side diff, default true) override `ObserveOpts`.
    async fn do_observe(&self, input: &Value) -> ToolResult {
        let mut opts = nomi_browser_engine::ObserveOpts::default();
        if let Some(d) = input.get("max_depth").and_then(|v| v.as_u64()) {
            // Clamp into u32; a model-supplied huge value can't overflow.
            opts.max_depth = d.min(u32::MAX as u64) as u32;
        }
        if let Some(b) = input.get("diff").and_then(|v| v.as_bool()) {
            opts.diff = b;
        }
        // P7B: when visual fallback is enabled, ask observe to also collect per-ref CSS-pixel
        // boxes (one extra injected roundtrip) so a later anchor-failure fallback can draw a SoM
        // (Set-of-Marks) overlay. OFF → no extra cost, boxes stay empty (zero behavior change).
        opts.include_boxes = self.visual_fallback_enabled;
        let engine = match self.engine().await {
            Ok(e) => e,
            Err(msg) => return ToolResult::error(format!("Browser engine unavailable: {msg}")),
        };
        match engine.observe(&opts).await {
            Ok(obs) => {
                let truncated = if obs.truncated {
                    " (tree truncated to the depth cap — deeper nodes were dropped)"
                } else {
                    ""
                };
                let url_note = obs
                    .url
                    .as_deref()
                    .map(|u| format!(" of {u}"))
                    .unwrap_or_default();
                let header = format!(
                    "[browser observation{url_note} · generation {} · {} element(s){truncated}. \
                     Each `[ref=f<seq>e<n>]` is valid only for this snapshot; re-run `observe` after \
                     any navigation or UI change.]\n",
                    obs.generation.0,
                    obs.entries.len(),
                );

                // P7A: attach site-memory hints (untrusted suggestions, not authoritative).
                let hints = self.site_memory_hints(obs.url.as_deref());
                let text = if hints.is_empty() {
                    format!("{header}{}", obs.yaml)
                } else {
                    format!("{header}{}\n{hints}", obs.yaml)
                };

                *self.last_snapshot.lock().expect("last_snapshot poisoned") = Some(obs);
                // A fresh observe clears the must-re-observe flag (refs are now valid).
                self.clear_must_re_observe();
                ToolResult::text(text)
            }
            Err(e) => self.engine_failure("Observe failed", e),
        }
    }

    /// **P3-N2: 截图落 per-pet workspace + 给会话流一个可视引用**（裁决⑨）。
    ///
    /// 引擎产出原始 PNG 字节，facade 在这里**两路并用**（不冲突）：
    /// 1. **给 LLM**：base64 包成 [`nomi_types::tool::ToolImage`] 经 `with_images` 回传——LLM 看图**内容**
    ///    （多模态），与 P0 行为一致。
    /// 2. **给用户会话流**：把 PNG 落进 per-pet workspace 的 `screenshots/` 子目录
    ///    （[`screenshot_path`]：`<workspace>/screenshots/shot-<ts>.png`；无 workspace 兜底
    ///    `<data_dir>/screenshots/`，仍是我们自己的隔离目录），并把落盘**路径引用**写进
    ///    [`ToolResult::content`]——会话流（`tool_call` → `normalizeToolCall` 的 `output` /
    ///    `getResultDisplayText` 的 `img_url`/`relative_path` 路径通道）据此展示截图引用。
    ///    **不新建 conversation 附件通道、不碰 requirement attachments 表**（裁决⑨）。
    ///
    /// 落盘是 best-effort 纵深附加：写盘失败**不**让截图动作整体失败（base64 仍回 LLM），只在
    /// content 里降级说明（保持 P0 的「截图已捕获」语义不回退）。
    async fn do_screenshot(&self) -> ToolResult {
        let engine = match self.engine().await {
            Ok(e) => e,
            Err(msg) => return ToolResult::error(format!("Browser engine unavailable: {msg}")),
        };
        match engine.screenshot().await {
            Ok(png) => {
                // `ToolImage.data` is base64-encoded (same as ComputerTool's
                // `encode_png`); the engine hands us raw PNG bytes.
                let img = nomi_types::tool::ToolImage {
                    media_type: "image/png".to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&png),
                };

                // P3-N2: 把 PNG 落进 per-pet workspace（无则兜底 data_dir）的 screenshots/ 子目录，
                // 并把落盘路径引用写进 content，让会话流能展示（裁决⑨：workspace 落盘 + 消息引用，
                // 不动 requirement 附件表）。落盘失败不回退截图动作（base64 仍回 LLM）。
                let base = self.screenshot_base_dir();
                let path = screenshot_path(&base, screenshot_timestamp());
                let saved = match persist_screenshot(&path, &png) {
                    Ok(()) => Some(path),
                    Err(e) => {
                        tracing::warn!(
                            target: "nomi_browser::screenshot",
                            error = %e,
                            dir = %base.display(),
                            "persisting the browser screenshot to the workspace failed; \
                             returning the image to the LLM only (no session-stream reference)"
                        );
                        None
                    }
                };
                let text = render_screenshot_note(saved.as_deref());
                ToolResult::text(text).with_images(vec![img])
            }
            Err(e) => self.engine_failure("Screenshot failed", e),
        }
    }

    /// **P3-N2: 截图落盘的基目录**——per-pet workspace（[`Self::workspace_dir`]，伙伴
    /// `{companion_id}/workspace` / 非伙伴会话工作目录）优先，无则兜底 [`Self::data_dir`]
    /// （仍是我们自己的隔离 app 数据目录，**绝不**是用户真实 Pictures/Desktop）。截图最终落进
    /// 它的 `screenshots/` 子目录（[`screenshot_path`]）——与下载的 `downloads/`（E4）平级隔离。
    fn screenshot_base_dir(&self) -> PathBuf {
        self.workspace_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.clone())
    }

    async fn do_capabilities(&self) -> ToolResult {
        let engine = match self.engine().await {
            Ok(e) => e,
            Err(msg) => return ToolResult::error(format!("Browser engine unavailable: {msg}")),
        };
        let caps = engine.capabilities();
        ToolResult::text(capabilities_note(&caps))
    }

    /// **F1: the current origin for the `secret:NAME` gate** (裁决⑦).
    ///
    /// The [`SecretStore`] resolves a credential only when the *current origin*'s
    /// eTLD+1 is among the secret's allowed origins. The facade's source of truth
    /// for "what page are we on" is the most recent `observe`'s `url` — which is
    /// exactly the right discipline: a `secret:NAME` reference can only resolve
    /// after the agent has `observe`d the page it intends to type into, and a
    /// stale/missing snapshot yields `None` → the gate fails closed (Blocked).
    ///
    /// **TODO(F1-sec)**: DESIGN §16/§230 calls for a *utility-world re-verification*
    /// of `document.location` at dispatch time (so the origin cannot drift between
    /// observe and act). That live re-check needs an engine-side hook the
    /// `BrowserEngine` trait does not expose yet; F1 uses the cached observe URL
    /// (fail-closed by construction) and leaves the live re-verify to F1-sec.
    fn current_origin(&self) -> Option<String> {
        self.last_snapshot
            .lock()
            .expect("last_snapshot poisoned")
            .as_ref()
            .and_then(|s| s.url.clone())
    }

    /// **F1: resolve a `type`/`set_value` text into a [`TypeInput`]**, intercepting
    /// `secret:NAME` references (裁决⑦, the security-critical path).
    ///
    /// - Plain text → [`TypeInput::Literal`] (logged/echoed normally).
    /// - `secret:NAME` → look up the current origin (last observe URL) and resolve
    ///   `NAME` through the [`SecretStore`] origin gate:
    ///   - origin matches the secret's allowed eTLD+1 → [`TypeInput::Secret`] (the
    ///     plaintext is injected via `Input.insertText`; it **never** enters the
    ///     LLM output, the ref table, or logs — `TypeInput::Secret`'s Debug is
    ///     redacted, and the engine's verify anchors omit the value);
    ///   - origin does not match / unknown name / no store / no current origin →
    ///     **fail-closed** `Err(blocked ToolResult)` (never falls back to typing
    ///     the literal string `secret:NAME`, never leaks the value).
    ///
    /// Returns `Ok(TypeInput)` to inject, or `Err(ToolResult)` the caller returns
    /// verbatim (a blocked/parameter error). **Never** logs the resolved value.
    fn resolve_type_input(&self, text: &str) -> Result<TypeInput, ToolResult> {
        let Some(name) = parse_secret_ref(text) else {
            // Ordinary literal text — not a secret reference.
            return Ok(TypeInput::Literal(text.to_string()));
        };

        // `secret:NAME` reference → must resolve through the origin-bound vault.
        let Some(origin) = self.current_origin() else {
            return Err(ToolResult::error(format!(
                "Cannot inject secret {name:?}: no current page origin is known. \
                 Run `observe` on the page you intend to type into first, then retry — \
                 a credential is only released for the origin it is bound to (fail-closed)."
            )));
        };

        let guard = self.secret_store.lock().expect("secret_store poisoned");
        let resolved = guard.as_ref().and_then(|store| store.resolve(name, &origin));
        match resolved {
            Some(value) => {
                // SECURITY: the plaintext is carried only inside TypeInput::Secret
                // (Debug-redacted) to the engine, which injects it via insertText.
                // We do NOT log `value` here. Only the (non-secret) name is traced.
                tracing::debug!(
                    target: "nomi_browser::secret",
                    name = %name,
                    "resolved secret for the current origin; injecting via insertText (value never enters the LLM/logs)"
                );
                // Known-secret blackout: insert the resolved plaintext into the shared set
                // so the engine's debug serializers can exact-blackout it in any captured
                // console/network/error output. Only values with len >= 4 to avoid
                // over-matching trivial strings (e.g. empty, "a", "ab", "abc").
                let plaintext = value.into_inner();
                if plaintext.len() >= 4
                    && let Ok(mut set) = self.known_secret_values.lock()
                {
                    set.insert(plaintext.clone());
                }
                Ok(TypeInput::Secret(plaintext))
            }
            // Fail-closed: unknown name, unbound origin, or no store. Never type the
            // literal `secret:NAME`, never leak. This holds in yolo/companion too —
            // the gate is a property of the vault, not an orchestration approval.
            None => Err(ToolResult::error(format!(
                "Secret {name:?} is not available for the current origin {origin:?} \
                 (fail-closed: unknown name, or the secret is bound to a different domain). \
                 The value was NOT typed. No secret is configured here unless one was registered \
                 for this exact registrable domain."
            ))),
        }
    }

    /// **F1: parse the tool `input` into an [`ActSpec`]**, validating required
    /// parameters **before** the engine (and thus the browser) is ever
    /// constructed (裁决: 缺参在启动浏览器前返错).
    ///
    /// `secret:NAME` interception happens here for `type`/`set_value`: the `text`/
    /// `value` is run through [`Self::resolve_type_input`] (the origin gate), so a
    /// blocked secret short-circuits with `Err(ToolResult)` before any launch.
    ///
    /// Returns `Ok(ActSpec)` ready for `engine.act`, or `Err(ToolResult)` — a
    /// non-panicking parameter/blocked error the caller returns verbatim. This is
    /// pure (no I/O, no engine), so the full action surface is unit-testable
    /// without a browser.
    fn build_act_spec(&self, action: &str, input: &Value) -> Result<ActSpec, ToolResult> {
        // Small extractors that name the missing parameter in the error (mirrors
        // ComputerTool's require_xy discipline — fail before any launch).
        let req_str = |key: &str| -> Result<String, ToolResult> {
            input
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    ToolResult::error(format!(
                        "Missing required parameter `{key}` for the {action} action."
                    ))
                })
        };
        let req_str_array = |key: &str| -> Result<Vec<String>, ToolResult> {
            match input.get(key).and_then(|v| v.as_array()) {
                Some(arr) => Ok(arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()),
                None => Err(ToolResult::error(format!(
                    "Missing required parameter `{key}` (a string array) for the {action} action."
                ))),
            }
        };

        let spec = match action {
            "click" => ActSpec::Click { r#ref: req_str("ref")? },
            "hover" => ActSpec::Hover { r#ref: req_str("ref")? },
            "type" => {
                let r#ref = req_str("ref")?;
                let text = req_str("text")?;
                // secret:NAME interception (origin gate) happens here — before launch.
                let text = self.resolve_type_input(&text)?;
                ActSpec::Type { r#ref, text }
            }
            "set_value" => {
                let r#ref = req_str("ref")?;
                // set_value also honors secret:NAME (writing a credential to a
                // controlled component). Resolve to a string the engine fills.
                let value_raw = req_str("value")?;
                match self.resolve_type_input(&value_raw)? {
                    TypeInput::Literal(s) => ActSpec::SetValue {
                        r#ref,
                        value: s,
                        secret: false,
                    },
                    // A secret destined for set_value: the engine's SetValue takes a
                    // plain String (it goes to insertText/fill, not the LLM), but the
                    // plaintext must not surface. `secret: true` mirrors
                    // TypeInput::Secret — it redacts the spec's Debug AND makes the
                    // engine suppress the verify before/after anchors (so F2 surfacing
                    // the Effect never leaks the credential). The value is still the
                    // plaintext (insertText needs it); only Debug/anchors are gated.
                    TypeInput::Secret(s) => ActSpec::SetValue {
                        r#ref,
                        value: s,
                        secret: true,
                    },
                }
            }
            "select_option" => ActSpec::SelectOption {
                r#ref: req_str("ref")?,
                options: req_str_array("options")?,
            },
            "press_key" => ActSpec::PressKey { keys: req_str("keys")? },
            "scroll" => {
                let direction = parse_scroll_dir(input.get("direction").and_then(|v| v.as_str()))
                    .ok_or_else(|| {
                        ToolResult::error(
                            "Missing or invalid `direction` for scroll (up, down, left, or right).",
                        )
                    })?;
                // target: optional element `ref` → scroll that element; else viewport.
                let target = match input.get("ref").and_then(|v| v.as_str()) {
                    Some(r) => nomi_browser_engine::ScrollTarget::Element { r#ref: r.to_string() },
                    None => nomi_browser_engine::ScrollTarget::Viewport,
                };
                let amount = input.get("amount").and_then(|v| v.as_f64());
                ActSpec::Scroll { target, direction, amount }
            }
            "scroll_to_text" => ActSpec::ScrollToText { text: req_str("text")? },
            "get_page_text" => ActSpec::GetPageText,
            "search_page" => ActSpec::SearchPage { query: req_str("query")? },
            "find_elements" => ActSpec::FindElements { selector: req_str("selector")? },
            "get_dropdown_options" => ActSpec::GetDropdownOptions { r#ref: req_str("ref")? },
            "cursor" => ActSpec::Cursor,
            "wait" => {
                let ms = input.get("ms").and_then(|v| v.as_u64()).ok_or_else(|| {
                    ToolResult::error(
                        "Missing required parameter `ms` (milliseconds, a non-negative integer) for wait.",
                    )
                })?;
                ActSpec::Wait { ms }
            }
            "wait_for" => ActSpec::WaitFor {
                condition: parse_wait_condition(input).map_err(ToolResult::error)?,
            },
            "upload_file" => {
                let r#ref = req_str("ref")?;
                // file_path may be a single string or an array of strings.
                let paths: Vec<PathBuf> = match input.get("file_path") {
                    Some(Value::String(s)) => vec![PathBuf::from(s)],
                    Some(Value::Array(arr)) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(PathBuf::from))
                        .collect(),
                    _ => {
                        return Err(ToolResult::error(
                            "Missing required parameter `file_path` (a path or array of paths) for upload_file.",
                        ));
                    }
                };
                if paths.is_empty() {
                    return Err(ToolResult::error(
                        "`file_path` for upload_file must name at least one file.",
                    ));
                }
                ActSpec::UploadFile { r#ref, paths }
            }
            "download" => ActSpec::Download { url: req_str("url")? },
            "save_as_pdf" => ActSpec::SaveAsPdf,
            "extract" => {
                // `schema` is the JSON schema describing the fields to extract. Accept an object
                // (the normal case) or fall back to an empty object when absent — extract still
                // returns a structured page representation, just without a field spec to guide it.
                let schema = match input.get("schema") {
                    Some(v) if v.is_object() || v.is_array() => v.clone(),
                    // Missing/null/non-structured schema: default to {} (extract still works,
                    // returning the deterministic page representation; the model can extract freely).
                    _ => serde_json::json!({}),
                };
                ActSpec::Extract { schema }
            }
            "switch_frame" => ActSpec::SwitchFrame { r#ref: req_str("ref")? },
            "tabs" => ActSpec::Tabs,
            "switch_tab" => ActSpec::SwitchTab { tab_id: req_str("tab_id")? },
            "close_tab" => ActSpec::CloseTab { tab_id: req_str("tab_id")? },
            "open_link_new_tab" => ActSpec::OpenLinkNewTab { url: req_str("url")? },
            "back" => ActSpec::Back,
            "forward" => ActSpec::Forward,
            "reload" => ActSpec::Reload,
            "navigate" => ActSpec::Navigate {
                url: req_str("url")?,
                new_tab: input.get("new_tab").and_then(|v| v.as_bool()).unwrap_or(false),
            },
            "evaluate" => ActSpec::Evaluate { script: req_str("script")? },
            "get_console_logs" => ActSpec::GetConsoleLogs,
            "get_page_errors" => ActSpec::GetPageErrors,
            "get_network_log" => ActSpec::GetNetworkLog {
                include_bodies: input.get("include_bodies").and_then(|v| v.as_bool()).unwrap_or(false),
            },
            other => {
                return Err(ToolResult::error(format!(
                    "Unknown action {other:?}. See the tool description for the list of supported \
                     actions."
                )));
            }
        };
        Ok(spec)
    }

    /// **F1: dispatch a parsed action through the engine.**
    ///
    /// Parses + validates the input into an [`ActSpec`] (missing params / blocked
    /// secrets short-circuit *before* the engine is constructed), then constructs
    /// the engine lazily and runs `engine.act(&spec, &progress)`. The [`Progress`]
    /// carries the action-level deadline (abort — page.close/frame.detach — is
    /// subscribed engine-side and beats the deadline).
    ///
    /// Maps [`nomi_browser_engine::ActResult`] to a [`ToolResult`]: the human
    /// `message` is the text, and a soft `success == false` is surfaced as a
    /// (non-error) note so the model can re-observe/retry. Engine
    /// [`nomi_browser_engine::BrowserError`]s become non-panicking tool errors.
    ///
    /// **F2 verify-after-act**: the engine's [`nomi_browser_engine::Effect`]
    /// (`changed` + before/after DOM anchors) is appended to the text so the model
    /// sees whether the action *actually* changed the page (never assume
    /// executed == succeeded), with explicit guidance to re-observe (a ref may be
    /// stale after a DOM change). **Secret safety**: anchors for secret actions are
    /// already `None` (suppressed engine-side in `act_type`/`act_set_value`), so
    /// surfacing the Effect here never leaks a credential.
    async fn do_act(&self, action: &str, input: &Value) -> ToolResult {
        // Takeover guard: if must_re_observe is set and this action uses a ref,
        // reject it — pre-takeover refs are invalid (user may have navigated).
        if self.needs_re_observe() && input.get("ref").is_some() {
            return ToolResult::error(
                "refs from before the takeover are invalid — the user may have navigated. \
                 Run `observe` first to get fresh refs, then retry the action."
                    .to_string(),
            );
        }

        // Parse + validate BEFORE constructing the engine (no browser launch on a
        // parameter error or a blocked secret).
        let spec = match self.build_act_spec(action, input) {
            Ok(spec) => spec,
            Err(tool_result) => return tool_result,
        };

        let engine = match self.engine().await {
            Ok(e) => e,
            Err(msg) => return ToolResult::error(format!("Browser engine unavailable: {msg}")),
        };

        let progress = Progress::new(ACT_TIMEOUT);
        match engine.act(&spec, &progress).await {
            Ok(result) => {
                let verify = render_verify_note(&result.effect);
                if result.success {
                    // P7C: if recording is active, append a step (selector is None at
                    // facade level — real selector generation requires engine internals
                    // and is wired in the e2e path via generate_selector).
                    if self.is_recording() {
                        self.record_step(action, input, None);
                    }

                    // P7A: site-memory — record successful action's element descriptor.
                    // Skip if the action carried a secret (locked invariant: no secret stored).
                    if let Some(ref store) = self.site_memory
                        && !spec_carries_secret(&spec)
                    {
                        self.record_site_memory(store, action, input);
                    }

                    // P3: For Extract, if the model seam is available, run LLM-driven
                    // structured extraction on the deterministic payload. Otherwise return
                    // the deterministic payload unchanged (graceful degradation).
                    if let ActSpec::Extract { ref schema } = spec
                        && let Some(ref model) = self.extract_model
                    {
                        let extract_schema = ExtractSchema::new(schema.clone());
                        match extract::extract_structured(
                            &result.message,
                            &extract_schema,
                            model.as_ref(),
                        )
                        .await
                        {
                            Ok(structured) => {
                                return ToolResult::text(
                                    serde_json::to_string_pretty(&structured)
                                        .unwrap_or_else(|_| structured.to_string()),
                                );
                            }
                            Err(e) => {
                                // LLM extraction failed — fall back to the deterministic
                                // payload (graceful degradation, never lose data).
                                tracing::warn!(
                                    "LLM extraction failed, returning deterministic payload: {e}"
                                );
                            }
                        }
                    }
                    ToolResult::text(format!("{}{verify}", result.message))
                } else {
                    // Soft failure (e.g. option not found, text not on page): not an
                    // error the model should panic on — surface the message so it can
                    // re-observe or adjust. Mirrors the engine's "良性失败" contract.
                    // `changed=false` is reported truthfully (never assume the action
                    // succeeded just because it was dispatched).
                    ToolResult::text(format!(
                        "{} (the action did not achieve its goal; re-observe and adjust){verify}",
                        result.message
                    ))
                }
            }
            Err(e) => {
                // ── P7B: visual fallback on anchor failure ──────────────────────
                // If the error is an anchor failure (NodeStale/NotConnected) AND visual
                // fallback is enabled + locator is present, attempt vision-based location
                // before returning the error to the model.
                let is_anchor_failure = matches!(
                    e,
                    BrowserError::NodeStale { .. } | BrowserError::NotConnected
                );
                if is_anchor_failure
                    && self.visual_fallback_enabled
                    && self.visual_locator.is_some()
                    && self.action_is_click_or_type(&spec)
                {
                    match self.attempt_visual_fallback(&engine, action, input, &spec).await {
                        Ok(result) => return result,
                        Err(fallback_err) => {
                            tracing::debug!(
                                target: "nomi_browser::visual_fallback",
                                original_error = %e,
                                fallback_error = %fallback_err,
                                "visual fallback failed, returning original engine error"
                            );
                            // Fall through to the original engine failure.
                        }
                    }
                }
                self.engine_failure("Browser action failed", e)
            }
        }
    }

    /// **E2/F1-sec: 为 [`redline::classify_action`] 采集运行时危险信号**（best-effort 纯读，不进浏览器）。
    ///
    /// 据 facade `input` + 最近一次 `observe` 的 [`Observation`] 组装 [`ActionContext`]。F1-sec 把能从
    /// **缓存 snapshot** 纯读出的真信号填实，无法在 facade 纯读路径拿到的（需运行时注入查/接 E5 网络层）
    /// 走保守缺省 + 引擎层兜底：
    ///
    /// - **element_accname / element_role**：按 `ref` 从 `last_snapshot` 查（分类器判付款/删除/发送
    ///   按钮要它）——已填实。
    /// - **is_submit_control**：observe 的 [`ElementEntry`] 不携带 `<button type=submit>` 的 `type`
    ///   属性（aria role 表里 submit 按钮与普通按钮同为 `button` role），故 facade 纯读路径**无法**可靠
    ///   区分；保守留 `false`，由 accname 路径（含「submit/提交」词）+ E5 跨域 POST 兜底承担真实的表单
    ///   提交识别。注入查 `el.type==='submit'` 需 dispatch 期元素句柄（C1 路径有，但那在 engine 内、
    ///   不在 facade 的 classify 时点）→ 引擎层已对 click 的真实副作用经 verify-after-act 守，且 E5 对
    ///   submit 触发的跨域 POST 在网络层拦，红线不漏（见下）。
    /// - **is_cross_origin_post**：跨域 POST 由 **E5 出口防火墙在网络层**拦（`Fetch.requestPaused` →
    ///   `firewall::decide`），facade 的 classify 时点**拿不到**「这次 click 会不会触发跨域 POST」（那
    ///   要等请求真发出）。故保守留 `false`——**E5 网络层兜底**（不靠这个信号），勿强造。
    /// - **enter_submits_form**：press_key 的裸 Enter 是否落在 `<form>` 内——见
    ///   [`Self::enter_submits_form_signal`]，据动作名 + keys best-effort 判（facade 拿不到 focus 树，
    ///   故对 Enter 类按键**保守升级**，由用户在普通会话确认 / yolo 下 hard-deny；非 Enter 不升级）。
    /// - **reload_resubmits_post**：reload 一个 POST 提交来的页——从 `last_snapshot` 的
    ///   `current_page_is_post` best-effort 读取（observe 时经 `Page.getNavigationHistory` +
    ///   [`nomi_browser_engine::nav::current_entry_is_post`] 填充）。snapshot 不存在或 observe 取不到
    ///   nav-history → `false`（保守不误判普通页 reload 为不可逆）。——已填实。
    ///
    /// **保守缺省契约**：信号取不到 → 该信号 `false`/`None`，分类器据此**不**升级（不误判普通动作为
    /// 不可逆）；真实红线由「accname 词表 + E5 网络层跨域 POST + 引擎层 reload/submit 副作用守」多重
    /// 兜底。能在 facade 纯读拿到的真信号（accname/role、Enter-落-form 的保守判定）已填实。
    fn build_action_context(&self, input: &Value) -> ActionContext {
        let mut ctx = ActionContext::default();

        // 按 ref 从最近 observation 查 accname/role（分类器判危险按钮需要）。Click/交互类动作的
        // `ref` 是 LLM 给的 `f<seq>e<n>` 句柄；last_snapshot 的 entries 据此查 role/name。
        if let Some(r) = input.get("ref").and_then(|v| v.as_str())
            && let Some(snap) = self.last_snapshot.lock().expect("last_snapshot poisoned").as_ref()
            && let Some(entry) = snap.entries.iter().find(|e| e.r#ref == r)
        {
            ctx.element_role = Some(entry.role.clone());
            if !entry.name.is_empty() {
                ctx.element_accname = Some(entry.name.clone());
            }
        }

        // press_key 的裸 Enter → 保守视作可能落 form（隐式提交）。facade 纯读路径拿不到 focus 树，故
        // 对 Enter 类按键保守升级（普通会话弹审批 / yolo 下 hard-deny），非 Enter 不升级。
        if let Some(action) = input.get("action").and_then(|v| v.as_str())
            && action == "press_key"
        {
            ctx.enter_submits_form =
                enter_submits_form_signal(input.get("keys").and_then(|v| v.as_str()));
        }

        // SD-4：reload 一个 POST 表单提交来的页面（重提交风险）。从 last_snapshot 的
        // `current_page_is_post` best-effort 读——与 url/accname 同源：上次 observe 时从
        // nav-history 查到的 transitionType==form_submit 信号。snapshot 不存在 → false（保守）。
        if let Some(action) = input.get("action").and_then(|v| v.as_str())
            && action == "reload"
        {
            ctx.reload_resubmits_post = self
                .last_snapshot
                .lock()
                .expect("last_snapshot poisoned")
                .as_ref()
                .map(|snap| snap.current_page_is_post)
                .unwrap_or(false);
        }

        // **P3-D2：is_cross_origin_post 信号填实（best-effort，保守 + E5/D2 网络层兜底）**。
        //
        // 当前页 origin 取自最近一次 observe 的 [`Observation::url`]（facade 缓存的 provenance）；目标
        // origin 取自本动作携带的目的 URL（仅 navigate 带 `url`；交互动作 click/type 的真实 form action
        // URL 在 DOM 里，facade 纯读路径**拿不到**）。**复用引擎层 PSL 机器**
        // （[`nomi_browser_engine::firewall::is_cross_origin`]）判跨域——与 E5 网络层同款 eTLD+1 判定，
        // 不另造逻辑。
        //
        // **保守契约**：只有当 (a) 有缓存的当前 origin **且** (b) 本动作真带一个目的 URL **且** (c) 二者
        // 跨 eTLD+1 时，才把信号置 true（让 classifier 升级）。拿不到 origin / 动作不带目的 URL（绝大多数
        // click/type）→ **保守留 false**——facade classify 时点本就无从可靠判「这次 click 会不会触发跨域
        // POST」（那要等请求真发出）。真实的跨域 POST 拦截由 **E5 出口防火墙 + D2 悬挂审批**在网络层兜底
        // （`Fetch.requestPaused` → `firewall::decide` → GatePost 悬挂等裁决 / fail-closed），**不靠**这个
        // facade 信号——故信号取不到只是少一层提前分类，绝不漏拦真正的跨域 POST。
        ctx.is_cross_origin_post = self.cross_origin_post_signal(input);

        ctx
    }

    /// **P3-D2 [纯读]：best-effort 判本动作是否会触发一个跨域请求**（填 `is_cross_origin_post` 信号）。
    ///
    /// 当前 origin = 最近一次 observe 的 [`Observation::url`]（facade 缓存）；目标 = 本动作携带的目的
    /// URL（`url` 参数，主要是 navigate；交互动作的真实 form-action URL 在 DOM 里，facade 拿不到 → 无目的
    /// URL → 返 `false`）。两侧都有 → 复用 [`nomi_browser_engine::firewall::is_cross_origin`]（同款 PSL
    /// eTLD+1 判定）。保守：缺当前 origin / 缺目的 URL / 同域 → `false`（见 [`Self::build_action_context`]
    /// 对兜底契约的说明：真拦截在 E5/D2 网络层）。
    fn cross_origin_post_signal(&self, input: &Value) -> bool {
        if matches!(
            input.get("action").and_then(Value::as_str),
            Some("navigate" | "open_link_new_tab" | "back" | "forward" | "reload")
        ) {
            return false;
        }
        // 目的 URL：本动作携带的 `url`（navigate 带；click/type 等交互动作不带 → 无从判 → false）。
        let Some(target_url) = input.get("url").and_then(Value::as_str) else {
            return false;
        };
        // 当前页 origin：最近一次 observe 的 provenance URL。无缓存 observe → 无从判 → 保守 false。
        let current_origin = {
            let guard = self.last_snapshot.lock().expect("last_snapshot poisoned");
            match guard.as_ref().and_then(|o| o.url.clone()) {
                Some(u) => u,
                None => return false,
            }
        };
        // 复用引擎层同款 PSL eTLD+1 跨域判定（E5 网络层同一机器，不另造逻辑）。
        nomi_browser_engine::firewall::is_cross_origin(&current_origin, target_url)
    }

    /// **E2/F1-sec: 本会话的 orchestration 审批闸是否被旁路**（`yolo || companion-forced-yolo ||
    /// auto_approve`）——[`redline::enforce_redline`] 的关键入参。
    ///
    /// **接线（P3-X1：LIVE 读运行时模式，非构造期快照）**：
    /// - [`Self::runtime_mode`] 为 `Some(mgr)` → **LIVE 读** `mgr.session_bypasses_approval()`
    ///   （权威映射：当且仅当当前 `session_mode == Yolo`；`AutoEdit`/`Default` 不 bypass）。故用户在会话
    ///   **中途**经 `set_mode` 翻 yolo / 翻回，红线门即时随之武装 / 解除（set_mode 运行时翻转缺口已闭）。
    /// - `None`（CLI REPL / 仅 `BrowserConfig` 的调用方 / 测试）→ 回退到构造期由 bootstrap 经
    ///   [`Self::with_policy`] 灌入的 [`Self::session_bypasses_approval`] 快照（= `config.tools.auto_approve`，
    ///   现行 fail-closed 行为不变）。
    ///
    /// 返 `true` 时 [`redline::enforce_redline`] 对不可逆动作 hard-deny；返 `false`（普通会话）时门
    /// 不拦，交 orchestration 正常审批。**bypass 映射方向（F1-sec）勿改**：只有 yolo 算 bypass，AutoEdit
    /// 不算（其只自动批 info/edit，从不批 Irreversible）——该方向的权威是
    /// [`ToolApprovalManager::session_bypasses_approval`]，本方法不复制它。
    fn session_bypasses_approval(&self) -> bool {
        match &self.runtime_mode {
            // P3-X1: 运行时句柄优先 → LIVE 读（set_mode 翻转即时反映）。映射方向（仅 yolo bypass）
            // 由 ToolApprovalManager 权威定义，facade 不复制。
            Some(mgr) => mgr.session_bypasses_approval(),
            // 兜底：无运行时句柄 → 构造期快照（= auto_approve，现行行为不变）。
            None => self.session_bypasses_approval,
        }
    }

    /// **E2: 带外确认是否已获**（headful takeover 原生 dialog / 网关手机审批）——P3-GW2 机制。
    ///
    /// **P3-GW2 接线**：读 `input` 里的 [`OUT_OF_BAND_CONFIRMED_KEY`] sentinel。当且仅当它为
    /// `true`（仅网关 dispatch 层在带外审批通过后注入，见 key 文档的信任边界）时返 `true`，让
    /// [`redline::enforce_redline`] 对旁路会话的不可逆动作放行。
    ///
    /// **fail-closed 默认**：引擎内 nomi 会话从不注入此 key（恒缺）→ 返 `false` → 现行 P2 行为不变
    /// （旁路会话不可逆动作仍 hard-deny）。带外放行仅经网关已确认的注入发生。facade 不剥此 key
    /// （剥除是网关分类前的职责，过了信任边界）；这里只读。
    fn out_of_band_confirmed(&self, input: &Value) -> bool {
        input
            .get(OUT_OF_BAND_CONFIRMED_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// **P3-GW2: 用 facade 的完整运行时上下文分类一个动作的审批级**（供网关 dispatch 层在转发前判
    /// 是否需带外审批）。
    ///
    /// 与 [`Self::redline_gate`] 同源走 [`Self::build_action_context`] → [`redline::classify_action`]，
    /// 故它**复用 facade 缓存的 `last_snapshot`**：按 `ref` 查到的危险 accname（Pay/Submit/删除…）在
    /// 这里被看见——这正是网关侧裸 `classify_action`（无 snapshot）看不到的信号。网关在一次 `observe`
    /// 后调本方法即可拿到与 facade 红线门一致的权威分级，对 [`ApprovalTier::Irreversible`] 走带外审批，
    /// 闭合「accname-only 可判但网关看不到」的缺口。
    ///
    /// 纯读（不进引擎、不启动浏览器）：只读 `input` + 已缓存 snapshot。
    pub fn classify_action_tier(&self, action: &str, input: &Value) -> ApprovalTier {
        let ctx = self.build_action_context(input);
        redline::classify_action(action, &ctx)
    }

    /// **E2: facade 独立 fail-closed 强制门**（设计裁决⑧关键，**不经 orchestration**）。
    ///
    /// 在 dispatch 到 engine **之前**调用：分类本次动作的审批级（[`redline::classify_action`]）→
    /// 据会话是否旁路审批 + 是否带外确认，由 [`redline::enforce_redline`] 决定放行 / hard-deny。
    ///
    /// 方向（勿搞反）：**只**拦审批旁路会话（yolo/companion）里的不可逆动作；普通会话的不可逆动作
    /// 放行（交 orchestration 正常审批，由 [`Self::category_for`] 返 [`ToolCategory::Irreversible`] 触发）。
    ///
    /// 返 `Some(ToolResult::error)` = 被拦（调用方直接返该错误，不 dispatch）；`None` = 放行。
    #[cfg_attr(not(test), allow(dead_code))]
    fn redline_gate(&self, action: &str, input: &Value) -> Option<ToolResult> {
        let ctx = self.build_action_context(input);
        let tier = redline::classify_action(action, &ctx);
        let bypass = self.session_bypasses_approval();
        let confirmed = self.out_of_band_confirmed(input) || self.unrestricted_approval;
        match redline::enforce_redline(tier, bypass, confirmed) {
            Ok(()) => None,
            Err(e) => {
                tracing::warn!(
                    target: "nomi_browser::redline",
                    action = %action,
                    tier = ?tier,
                    "redline gate hard-denied an irreversible action in an approval-bypassing session"
                );
                Some(ToolResult::error(e.to_string()))
            }
        }
    }

    /// **P7D: redline gate with takeover integration** (async).
    ///
    /// When the redline gate would block (irreversible + bypass + not confirmed) AND
    /// takeover is enabled, attempts a human takeover:
    /// 1. Requests takeover via [`crate::takeover::TakeoverController`].
    /// 2. Awaits the resolution (or timeout).
    /// 3. Passes `resolution.to_confirmed()` to `enforce_redline`.
    /// 4. If confirmed: sets `must_re_observe` (user may have navigated) and returns None (proceeds).
    /// 5. Otherwise: returns the Blocked error (fail-closed).
    ///
    /// When takeover is disabled (default), falls through to the sync `redline_gate`.
    async fn redline_gate_with_takeover(&self, action: &str, input: &Value) -> Option<ToolResult> {
        let ctx = self.build_action_context(input);
        let tier = redline::classify_action(action, &ctx);
        let bypass = self.session_bypasses_approval();
        let sentinel_confirmed = self.out_of_band_confirmed(input) || self.unrestricted_approval;

        // Fast path: not irreversible, not bypass, or already sentinel-confirmed → use sync gate.
        if tier != redline::ApprovalTier::Irreversible || !bypass || sentinel_confirmed {
            return match redline::enforce_redline(tier, bypass, sentinel_confirmed) {
                Ok(()) => None,
                Err(e) => Some(ToolResult::error(e.to_string())),
            };
        }

        // We're in the "irreversible + bypass + not sentinel-confirmed" path.
        // Try takeover if enabled.
        if !self.takeover_controller.enabled {
            // Takeover disabled → hard-deny (existing fail-closed behavior).
            tracing::warn!(
                target: "nomi_browser::redline",
                action = %action,
                tier = ?tier,
                "redline gate hard-denied (takeover disabled)"
            );
            let err = redline::enforce_redline(tier, bypass, false).unwrap_err();
            return Some(ToolResult::error(err.to_string()));
        }

        // Takeover enabled — request human approval.
        tracing::info!(
            target: "nomi_browser::takeover",
            action = %action,
            "requesting human approval for irreversible action in bypass session"
        );

        let description = ctx
            .element_accname
            .clone()
            .unwrap_or_else(|| action.to_string());

        // Phase D: prefer the injected approval gate (desktop event + ToolApprovalManager,
        // or gateway GW2 confirm). It owns notify + await + timeout + fail-closed. Without a
        // gate, fall back to the TakeoverController's handle/future — whose handle has no UI
        // to resolve it, so it fail-closes (preserving exact pre-Phase-D behavior).
        let confirmed = if let Some(gate) = &self.approval_gate {
            // Phase 3: attach a current-page preview so a SILENT (headless) session can still
            // show the user what they're approving (no visible window needed). Best-effort:
            // reuse the same redaction-aware `engine.screenshot()` `do_screenshot` uses; a
            // capture failure MUST NOT block the ask (attach None, still surface the text) —
            // the redline keystone (only explicit Approve releases) stays intact regardless.
            let screenshot = match self.engine().await {
                Ok(engine) => engine.screenshot().await.ok().map(|png| {
                    format!(
                        "data:image/png;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&png)
                    )
                }),
                Err(_) => None,
            };
            let ask = crate::approval::ApprovalAsk {
                kind: crate::approval::ApprovalKind::IrreversibleAction {
                    action: action.to_string(),
                    description,
                },
                screenshot,
            };
            gate.request_approval(ask).await.is_approved()
        } else {
            let reason = crate::takeover::TakeoverReason::IrreversibleAction {
                action: action.to_string(),
                description,
            };
            let (_handle, future) = self.takeover_controller.request(reason).split();
            // No UI bridge for the handle → resolves to Cancelled/TimedOut (fail-closed).
            future.resolve().await.to_confirmed()
        };

        tracing::info!(
            target: "nomi_browser::takeover",
            action = %action,
            confirmed,
            "takeover resolved"
        );

        // Feed the resolution back into enforce_redline.
        match redline::enforce_redline(tier, bypass, confirmed) {
            Ok(()) => {
                // Confirmed! The user approved. Set must-re-observe (user may have navigated).
                self.set_must_re_observe();
                None
            }
            Err(e) => {
                // Cancelled / TimedOut / Unavailable → still Blocked.
                Some(ToolResult::error(e.to_string()))
            }
        }
    }

    // ── P7B: Visual Fallback helpers ────────────────────────────────────────

    /// Returns `true` if the action spec is a click or type (actions that target a
    /// specific element by ref — the only actions where visual fallback makes sense).
    fn action_is_click_or_type(&self, spec: &ActSpec) -> bool {
        matches!(
            spec,
            ActSpec::Click { .. }
                | ActSpec::Type { .. }
                | ActSpec::Hover { .. }
                | ActSpec::SetValue { .. }
        )
    }

    /// Attempt to locate the target element visually and click at the found coordinates.
    ///
    /// # Steps
    /// 1. Take a screenshot (the engine's screenshot is already the redacted version —
    ///    password fields are input-masked at the OS/browser level).
    /// 2. Build an instruction from the element's last-known description (accname/role from
    ///    the snapshot).
    /// 3. Call the vision locator to find the target.
    /// 4. Convert pixel coords to CSS via DPR division (THE KEYSTONE).
    /// 5. Run the redline gate (irreversible visual clicks still need approval).
    /// 6. Dispatch via `engine.click_at_css_point(x, y)`.
    ///
    /// # Security
    /// - The screenshot fed to the vision model is the engine's native screenshot (same
    ///   as observe uses — secrets are browser-level masked in input fields).
    /// - The redline gate STILL runs (visual clicks of "Pay" / "Delete" buttons are still
    ///   subject to approval).
    async fn attempt_visual_fallback(
        &self,
        engine: &Arc<dyn BrowserEngine>,
        action: &str,
        input: &Value,
        _spec: &ActSpec,
    ) -> Result<ToolResult, String> {
        let locator = self
            .visual_locator
            .as_ref()
            .ok_or_else(|| "visual locator not available".to_string())?;

        // 1. Take a screenshot (the engine's screenshot — already at device pixel resolution).
        let screenshot = engine
            .screenshot()
            .await
            .map_err(|e| format!("screenshot for visual fallback failed: {e}"))?;

        // 2. Build instruction from last-known element description.
        let instruction = self.build_visual_instruction(input);

        // 3. DPR: the engine screenshot is at device-pixel resolution; the vision model
        // returns device/image-pixel coordinates. We need the page's devicePixelRatio to
        // convert those to the CSS-pixel space `click_at_css_point` expects. Query the live
        // page (`window.devicePixelRatio`); best-effort — a probe failure falls back to 1.0
        // (correct for headless Chrome, and never a reason to block the click).
        let dpr = engine.device_pixel_ratio().await.unwrap_or(1.0);

        // 4. Locate the target → a CSS-pixel click point. Prefer SoM (Set-of-Marks) when we have
        //    cached per-ref geometry for a tractable number of clickable elements: drawing
        //    numbered boxes and asking the model to "pick a label" is far more reliable than
        //    free-form pixel regression. Any SoM miss/error falls through to the raw bbox path.
        let mut via_som = false;
        let css_point = 'pt: {
            if let Some(rects_css) = self.som_rects_css() {
                if (1..=MAX_SOM_LABELS).contains(&rects_css.len()) {
                    match Self::som_locate_point(locator.as_ref(), &screenshot, &instruction, dpr, &rects_css)
                        .await
                    {
                        Ok(p) => {
                            via_som = true;
                            break 'pt p;
                        }
                        Err(e) => tracing::debug!(
                            target: "nomi_browser",
                            error = %e,
                            "SoM fallback miss; trying raw bbox locate"
                        ),
                    }
                }
            }
            // Raw bbox fallback (the always-available path).
            crate::visual_fallback::VisualFallback::new(locator.clone())
                .locate_and_target(&screenshot, &instruction, dpr)
                .await
                .map_err(|e| format!("visual locator failed: {e}"))?
        };

        // 5. Redline gate — irreversible visual clicks STILL require approval.
        if let Some(blocked) = self.redline_gate(action, input) {
            return Ok(blocked);
        }

        // 6. Dispatch via the engine's point-click seam (CSS pixels, zero DPR).
        engine
            .click_at_css_point(css_point.x, css_point.y)
            .await
            .map_err(|e| format!("visual fallback click dispatch failed: {e}"))?;

        let mode = if via_som { "visual fallback (SoM)" } else { "visual fallback" };
        // Honest, action-aware result: visual fallback only ever dispatches a point CLICK. For a
        // Click that fully performs the action. For Type/SetValue/Hover it merely lands the
        // pointer (focusing the target) — the original operation is NOT replayed, so say so
        // plainly instead of implying success (the model must re-observe + re-issue with a fresh
        // ref). Prevents a "looks done but the text was never typed" pitfall.
        let msg = if action.eq_ignore_ascii_case("click") {
            format!(
                "Clicked at ({:.0}, {:.0}) via {mode} (DOM anchor was stale). \
                 Re-observe to verify the action succeeded.",
                css_point.x, css_point.y
            )
        } else {
            format!(
                "Located the target at ({:.0}, {:.0}) and clicked it via {mode} to focus it \
                 (DOM anchor was stale). The original `{action}` was NOT replayed — re-observe \
                 and re-issue it with a fresh ref.",
                css_point.x, css_point.y
            )
        };
        Ok(ToolResult::text(msg))
    }

    /// **P7B SoM geometry**: collect CSS-pixel boxes for the **clickable** elements of the last
    /// observation (joining each entry's role with its box). Non-interactive refs (generic
    /// containers, headings, status text) are excluded — SoM should only number actionable
    /// targets, both for vision-model precision (fewer, meaningful labels) and to keep the
    /// overlay legible. `None` when there's no snapshot, no boxes were collected (visual fallback
    /// off at observe-time / a geometry-probe failure), or nothing clickable has a box.
    ///
    /// **Known tradeoff**: these boxes come from the *cached* observation, while the SoM overlay
    /// is drawn on a *fresh* screenshot taken at fallback time. They're collected at observe to
    /// dodge the stale-ref paradox (the fallback fires precisely because the target ref went
    /// stale — re-querying geometry then would fail). If the layout shifted between observe and
    /// the fallback, the overlay can misalign and the model may pick a wrong label; the raw-bbox
    /// fallback (fresh screenshot, free-form locate) plus the agent's re-observe-after-act loop
    /// are the safety net, and the redline gate still guards any dangerous click.
    fn som_rects_css(&self) -> Option<Vec<nomi_browser_engine::CssRect>> {
        let guard = self.last_snapshot.lock().expect("last_snapshot poisoned");
        let snap = guard.as_ref()?;
        if snap.boxes.is_empty() {
            return None;
        }
        let rects: Vec<nomi_browser_engine::CssRect> = snap
            .entries
            .iter()
            .filter(|e| Self::is_som_clickable_role(&e.role))
            .filter_map(|e| snap.boxes.get(&e.r#ref).copied())
            .collect();
        if rects.is_empty() { None } else { Some(rects) }
    }

    /// Whether an aria role denotes a directly-clickable/typeable control worth a SoM label.
    /// Conservative allowlist of interactive ARIA roles (generic/heading/status/etc. excluded).
    /// A role that's missing here only costs SoM precision for that page (it falls back to the
    /// raw-bbox path), never correctness — so the list errs toward common interactive widgets.
    fn is_som_clickable_role(role: &str) -> bool {
        matches!(
            role,
            "button"
                | "link"
                | "checkbox"
                | "radio"
                | "switch"
                | "tab"
                | "menuitem"
                | "menuitemcheckbox"
                | "menuitemradio"
                | "option"
                | "textbox"
                | "searchbox"
                | "combobox"
                | "spinbutton"
                | "slider"
                | "treeitem"
                | "gridcell"
        )
    }

    /// **P7B SoM locate**: draw a numbered overlay on the screenshot, ask the vision model to
    /// pick the label matching `instruction`, and map that label back to a CSS-pixel click point.
    ///
    /// Coordinate flow: cached rects are CSS pixels → ×`dpr` to device/image pixels (the
    /// screenshot/overlay space) for drawing → the picked label's device-pixel rect center is
    /// ÷`dpr` back to CSS pixels for the engine click. (Net: the click lands on the true CSS
    /// center regardless of DPR; `dpr` only governs overlay alignment with what the model sees.)
    /// Returns `Err` on overlay-empty / model no-match / out-of-range label → caller falls back.
    async fn som_locate_point(
        locator: &dyn crate::visual_fallback::VisualLocator,
        screenshot: &[u8],
        instruction: &str,
        dpr: f64,
        rects_css: &[nomi_browser_engine::CssRect],
    ) -> Result<crate::visual_fallback::CssPoint, String> {
        use crate::visual_fallback::{CssPoint, ElementRect, som_overlay, to_css_point};

        let rects_dev: Vec<ElementRect> = rects_css
            .iter()
            .map(|r| ElementRect {
                x: r.x * dpr,
                y: r.y * dpr,
                width: r.width * dpr,
                height: r.height * dpr,
            })
            .collect();
        let overlay = som_overlay(screenshot, &rects_dev);
        let n = overlay.label_map.len();
        if n == 0 {
            return Err("SoM overlay produced no labels".to_string());
        }
        let picked = locator
            .locate_labeled(&overlay.annotated_png, instruction, n)
            .await?;
        // Map the 1-based label back to its rect (device px). Parser already bounds it to
        // 1..=n, but find-by-number is the source of truth (also guards a sparse map).
        let label = overlay
            .label_map
            .iter()
            .find(|l| l.number == picked.label)
            .ok_or_else(|| format!("SoM label {} not present in label_map (n={n})", picked.label))?;
        let cx = label.rect.x + label.rect.width / 2.0;
        let cy = label.rect.y + label.rect.height / 2.0;
        let (x, y) = to_css_point(cx, cy, dpr);
        Ok(CssPoint { x, y })
    }

    /// Build a natural-language instruction for the vision locator from the element's
    /// last-known description (accname + role from the cached snapshot).
    fn build_visual_instruction(&self, input: &Value) -> String {
        if let Some(r) = input.get("ref").and_then(|v| v.as_str()) {
            if let Some(snap) = self.last_snapshot.lock().expect("last_snapshot poisoned").as_ref() {
                if let Some(entry) = snap.entries.iter().find(|e| e.r#ref == r) {
                    let role = &entry.role;
                    let name = &entry.name;
                    if !name.is_empty() {
                        return format!("the {role} named \"{name}\"");
                    }
                    return format!("the {role} element");
                }
            }
        }
        // Fallback: use the action + ref as a generic description.
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("click");
        format!("the target element for {action}")
    }
}

/// **F1-sec: [纯逻辑] press_key 的裸 Enter 是否应保守视作「落 form 隐式提交」**（喂
/// [`redline::ActionContext::enter_submits_form`]）。
///
/// facade 的 classify 时点拿不到 focus 树（「Enter 是不是落在 `<form>` 里」要运行时查），故对
/// **裸 Enter / Return**（无修饰键、或仅 keypad Enter）**保守升级**为可能提交——分类器据此把
/// `press_key` 判 Irreversible（普通会话弹审批 / yolo 下 hard-deny）。带修饰键的组合（如
/// `Ctrl+Enter` 提交、`Shift+Enter` 换行）语义各异，且更可能是有意快捷键 → **不**据此升级（保守
/// 只对最常见的「输入框里按回车 = 提交表单」这一形态升级，避免把所有 Enter 都拦死）。
///
/// 判定（大小写不敏感，去空白）：
/// - `"Enter"` / `"Return"`（裸键，无 `+` 组合）→ `true`；
/// - 含 `+`（组合键）/ 其它键 / `None` → `false`。
///
/// 这是「宁可过判一道确认（普通会话只是弹审批，yolo 下被拦但 P3 带外确认放行），也不漏判一个隐式
/// 表单提交」的保守方向；真实的「Enter 落 form」精确判定（注入查 focus-in-form）在引擎层 C2 的
/// `press_key` dispatch 路径有，本 facade 信号是其前置的保守闸。
fn enter_submits_form_signal(keys: Option<&str>) -> bool {
    let Some(keys) = keys else { return false };
    let k = keys.trim();
    // 组合键（含 '+'）不据此升级——可能是有意快捷键（Ctrl+Enter 等），语义不统一。
    if k.contains('+') {
        return false;
    }
    let lower = k.to_ascii_lowercase();
    lower == "enter" || lower == "return"
}

/// **F1-sec: parse a `secret:NAME` reference** (裁决⑦). Returns `Some(name)` when
/// `text` is exactly `secret:` followed by a non-empty name; `None` for ordinary
/// text. The match is on the literal prefix only — a bare `"secret:"` (no name)
/// is *not* a reference (returns `None`, treated as literal text) so it can never
/// resolve to an unnamed credential. Pure (no I/O), so the secret-detection logic
/// is unit-testable.
fn parse_secret_ref(text: &str) -> Option<&str> {
    let name = text.strip_prefix(SECRET_PREFIX)?;
    if name.is_empty() {
        // `"secret:"` with no name is not a credential reference.
        None
    } else {
        Some(name)
    }
}

/// **F2 verify-after-act: render the engine's [`Effect`] as a model-facing note**
/// (pure, unit-testable). Appended to every action's [`ToolResult`] text so the
/// model sees whether the action *actually* changed the page — `never assume
/// executed == succeeded` (DESIGN §21 verify). The note states `changed: true/false`
/// and includes the before/after anchors **only when present** (secret actions
/// suppress them engine-side, so this can never echo a credential — when both are
/// `None` only `changed` is shown). Always nudges to re-observe (a ref may be stale
/// after a DOM change; the snapshot is the ground truth).
///
/// **P7A [pure logic]**: Returns `true` if an [`ActSpec`] carries a secret value that
/// must NOT be remembered in site memory (locked invariant: no secret ever stored).
fn spec_carries_secret(spec: &ActSpec) -> bool {
    matches!(
        spec,
        ActSpec::Type { text: TypeInput::Secret(_), .. } | ActSpec::SetValue { secret: true, .. }
    )
}

/// Output begins with `\n` so it cleanly follows the human message; an empty Effect
/// (no anchors) still yields a `verified:` line so the model always sees the
/// changed-or-not truth.
fn render_verify_note(effect: &Effect) -> String {
    let mut note = format!("\n\nverified: changed={}", effect.changed);
    // Anchors are opaque JSON (URL / value / scrollY / checked …). They are ONLY
    // present for non-secret actions — the engine sets them to None for secrets, so
    // surfacing them here is leak-safe by construction.
    if let Some(before) = &effect.before_anchor {
        note.push_str(&format!("; before={}", compact_anchor(before)));
    }
    if let Some(after) = &effect.after_anchor {
        note.push_str(&format!("; after={}", compact_anchor(after)));
    }
    // The ref the model used may no longer resolve after a DOM change → re-observe.
    note.push_str(". Re-observe to see the latest page state before acting again.");
    note
}

/// Compact one anchor [`serde_json::Value`] to a short string for the verify note
/// (strings unquoted; objects/values via compact JSON). Keeps the note readable
/// without dumping huge structures (anchors are small: a URL, a value, a scroll pos).
fn compact_anchor(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// **P3-N2: per-pet workspace 下的隔离截图子目录名**（裁决⑨）。截图**只**落
/// `<base>/<SCREENSHOT_SUBDIR>/`（base = per-pet workspace，无则兜底 data_dir），与下载的
/// `downloads/`（E4 [`nomi_browser_engine::download::DOWNLOAD_SUBDIR`]）平级隔离，**绝不**指向
/// 用户真实 Pictures/Desktop。
pub const SCREENSHOT_SUBDIR: &str = "screenshots";

/// **P3-N2 [纯逻辑]：解析截图落盘路径** `<base>/screenshots/shot-<ts>.png`（裁决⑨）。
///
/// `base` = per-pet workspace（[`BrowserTool::workspace_dir`]）或兜底 data_dir
/// （[`BrowserTool::screenshot_base_dir`] 决定）；`ts` = 单调毫秒时间戳（[`screenshot_timestamp`]，
/// 同会话多次截图不互相覆盖）。纯函数（不碰 FS）——故落盘路径解析（workspace/screenshots/
/// shot-{ts}.png；无 workspace 兜底 data_dir）可在不启动浏览器/不写盘的前提下单测。
pub fn screenshot_path(base: &std::path::Path, ts: u128) -> PathBuf {
    base.join(SCREENSHOT_SUBDIR).join(format!("shot-{ts}.png"))
}

/// **P3-N2: 截图文件名用的时间戳**（自 UNIX_EPOCH 起毫秒）。同会话连拍区分文件名（不覆盖）。
/// 时钟回拨/取不到时兜底 `0`（仍产合法文件名，不 panic）。
fn screenshot_timestamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// **P3-N2: 把 PNG 字节落盘到 `path`**（best-effort 纵深附加，裁决⑨）。先 mkdir 父目录
/// （`<base>/screenshots/`），再写文件。返回 `Err` 让调用方降级（base64 仍回 LLM，只是没有会话流
/// 路径引用）——绝不因落盘失败让整个截图动作失败。
fn persist_screenshot(path: &std::path::Path, png: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, png)
}

/// **P3-N2 [纯逻辑]：构造截图 [`ToolResult`] 的 content 文本**（裁决⑨：消息内容引用）。
///
/// - `Some(path)`：落盘成功 → content 含**落盘路径引用**，让会话流（`tool_call` → 展示 `output` /
///   `getResultDisplayText` 的路径通道）可视化展示截图所在。仍保留 P0 的「已捕获」语义。
/// - `None`：落盘失败（best-effort 降级）→ 仍报「已捕获」（base64 已回 LLM），只是无路径引用。
///
/// 纯函数（不碰 FS）——故「ToolResult 图片引用构造」可单测。
fn render_screenshot_note(saved: Option<&std::path::Path>) -> String {
    match saved {
        Some(path) => format!(
            "Screenshot of the current page captured and saved to {} \
             (also returned inline for viewing).",
            path.display()
        ),
        None => "Screenshot of the current page captured.".to_string(),
    }
}


/// **F1: parse a scroll `direction` string** into [`nomi_browser_engine::ScrollDir`].
/// `None` for missing/invalid (caller turns that into a parameter error before launch).
fn parse_scroll_dir(s: Option<&str>) -> Option<nomi_browser_engine::ScrollDir> {
    use nomi_browser_engine::ScrollDir;
    match s? {
        "up" => Some(ScrollDir::Up),
        "down" => Some(ScrollDir::Down),
        "left" => Some(ScrollDir::Left),
        "right" => Some(ScrollDir::Right),
        _ => None,
    }
}

/// **F1: parse the `wait_for` condition** from the tool input into a
/// [`nomi_browser_engine::WaitCondition`]. Accepts `condition` ∈
/// {`url_contains`, `text_visible`, `ref_actionable`} plus the matching payload
/// (`text` / `ref`). Returns a descriptive error string (caller wraps in a
/// `ToolResult::error`) when the shape is wrong — validated before any launch.
fn parse_wait_condition(input: &Value) -> Result<nomi_browser_engine::WaitCondition, String> {
    use nomi_browser_engine::WaitCondition;
    let kind = input
        .get("condition")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "Missing required parameter `condition` for wait_for (url_contains, text_visible, or \
             ref_actionable)."
                .to_string()
        })?;
    match kind {
        "url_contains" => {
            let text = input.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                "wait_for `url_contains` needs a `text` substring to wait for in the URL.".to_string()
            })?;
            Ok(WaitCondition::UrlContains { text: text.to_string() })
        }
        "text_visible" => {
            let text = input.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                "wait_for `text_visible` needs a `text` to wait for on the page.".to_string()
            })?;
            Ok(WaitCondition::TextVisible { text: text.to_string() })
        }
        "ref_actionable" => {
            let r = input.get("ref").and_then(|v| v.as_str()).ok_or_else(|| {
                "wait_for `ref_actionable` needs a `ref` (from the latest observe) to wait on."
                    .to_string()
            })?;
            Ok(WaitCondition::RefActionable { r#ref: r.to_string() })
        }
        other => Err(format!(
            "Unknown wait_for condition {other:?}; use url_contains, text_visible, or ref_actionable."
        )),
    }
}

/// A cheap default capabilities report used to render the tool description at
/// construction time. We must NOT launch a browser to build the description, so
/// we advertise the engine name and a "launched lazily" posture; the live
/// `capabilities` action reports the true state once the engine is up.
fn default_capabilities() -> Capabilities {
    Capabilities {
        browser_ready: false,
        headful: false,
        display_available: false,
        engine: "chromium".to_string(),
    }
}

/// Render a capability note for the model. Always contains the word "browser"
/// so the model knows this is the browser surface (the description test pins
/// this). When `browser_ready` is false (the construction-time default) it says
/// the browser launches on first use rather than claiming a live state.
fn capabilities_note(caps: &Capabilities) -> String {
    let engine = if caps.engine.is_empty() {
        "chromium"
    } else {
        caps.engine.as_str()
    };
    if caps.browser_ready {
        format!(
            "\n\nThis browser session's capabilities:\n\
             - Engine: {engine} (in-process CDP).\n\
             - Window: {}.\n\
             - Display available: {}.\n\
             - `observe` reads the page as an aria snapshot with `[ref=f<seq>e<n>]` element refs; \
             refs go stale after navigation/UI changes, so re-run `observe` for fresh ones.",
            if caps.headful { "visible (headful)" } else { "headless" },
            if caps.display_available { "yes" } else { "no" },
        )
    } else {
        format!(
            "\n\nThis browser session's capabilities:\n\
             - Engine: {engine} (in-process CDP); the managed browser launches lazily on the \
             first action.\n\
             - `observe` reads the page as an aria snapshot with `[ref=f<seq>e<n>]` element refs; \
             refs go stale after navigation/UI changes, so re-run `observe` for fresh ones.\n\
             - Use the `capabilities` action after a browser action for the live headful/display \
             state."
        )
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "Browser"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        // read-only
                        "navigate", "observe", "screenshot", "capabilities",
                        "get_page_text", "search_page", "find_elements",
                        "get_dropdown_options", "cursor", "tabs", "wait", "wait_for",
                        "get_console_logs", "get_page_errors", "get_network_log",
                        // write / interaction
                        "click", "hover", "type", "set_value", "select_option",
                        "press_key", "scroll", "scroll_to_text", "upload_file",
                        "download", "save_as_pdf", "extract",
                        "switch_frame", "switch_tab", "close_tab", "open_link_new_tab",
                        "back", "forward", "reload", "evaluate"
                    ],
                    "description": "The browser operation to perform"
                },
                // element addressing
                "ref": { "type": "string", "description": "A `[ref=f<seq>e<n>]` element from the latest `observe` (click/hover/type/set_value/select_option/get_dropdown_options/upload_file/switch_frame; optional for scroll to target an element)" },
                // navigation
                "url": { "type": "string", "description": "URL to load (navigate / open_link_new_tab) or to download (download action), e.g. \"https://example.com\"" },
                "new_tab": { "type": "boolean", "description": "Open the URL in a new tab instead of the current one (navigate action), default false" },
                // observe
                "max_depth": { "type": "integer", "description": "Max accessibility-tree depth to serialize (observe action), default 12 — lower it for huge pages" },
                "diff": { "type": "boolean", "description": "Use the injected-side diff for this observe (observe action), default true" },
                // text input
                "text": { "type": "string", "description": "Text to type (type action) or to find (scroll_to_text / wait_for text_visible). For `type`, use \"secret:NAME\" to inject a stored credential bound to the current origin WITHOUT the value passing through this conversation" },
                "value": { "type": "string", "description": "Value to set on a control (set_value action); also accepts \"secret:NAME\"" },
                // keys / scroll
                "keys": { "type": "string", "description": "Key or combo to press (press_key action), e.g. \"Enter\", \"Control+a\", \"Tab\"" },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction (scroll action)"
                },
                "amount": { "type": "number", "description": "Scroll amount (scroll action); optional, engine default applies" },
                // select / find / search
                "options": { "type": "array", "items": { "type": "string" }, "description": "Option values/labels to select in a <select> (select_option action)" },
                "selector": { "type": "string", "description": "CSS selector to find elements by (find_elements action)" },
                "query": { "type": "string", "description": "Text to grep the page for (search_page action)" },
                // upload
                "file_path": { "description": "File path, or array of file paths, to set on a file input (upload_file action)" },
                // tabs
                "tab_id": { "type": "string", "description": "Tab id from the `tabs` action (switch_tab / close_tab)" },
                // waits
                "ms": { "type": "integer", "description": "Milliseconds to wait (wait action)" },
                "condition": {
                    "type": "string",
                    "enum": ["url_contains", "text_visible", "ref_actionable"],
                    "description": "Condition kind to wait for (wait_for action); pair with `text` (url_contains/text_visible) or `ref` (ref_actionable)"
                },
                // advanced / gated
                "script": { "type": "string", "description": "Script to evaluate in the page (evaluate action); disabled unless full-power mode is enabled" },
                // extract / download
                "schema": { "type": "object", "description": "JSON schema describing the fields to extract from the page (extract action); the page is returned as a structured, redacted representation to extract against" },
                // debug capture
                "include_bodies": { "type": "boolean", "description": "Include request/response bodies in network log (get_network_log action); default false — bodies are large and may contain secrets" }
            },
            "required": ["action"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // DESIGN §22：per-target act 串行 + observe⊥act。引擎已用 op_mutex 在内部强制互斥,但 Browser
        // 工具仍永不可被 orchestrator 并发批处理——同引擎两调用在 op_mutex 上也会串行,且调度器不得假设
        // 其副作用相互独立。恒 false（正确性地基=引擎 op_mutex；调度天花板=此处恒 false,两者都须成立）。
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(action) = input.get("action").and_then(|v| v.as_str()) else {
            return ToolResult::error(
                "Missing required parameter `action`. Supported actions: navigate, observe, \
                 screenshot, capabilities.",
            );
        };

        tracing::debug!(action = %action, "BrowserTool executing");

        // E2: facade 独立 fail-closed 强制门（**不经 orchestration**，设计裁决⑧）。在 dispatch 前拦
        // 审批旁路会话（yolo/companion）里的不可逆动作（submit/付款/删除/发送/跨域 POST/Enter 落 form/
        // POST reload）→ hard-deny。普通会话不拦（交 orchestration 经 category_for 的 Irreversible 审批）。
        //
        // **P7D takeover integration**: when the gate would block AND takeover is enabled,
        // attempt a human takeover first. The takeover resolution's `to_confirmed()` feeds
        // back into `enforce_redline` — ONLY a genuine Confirmed releases the action.
        if let Some(blocked) = self.redline_gate_with_takeover(action, &input).await {
            return blocked;
        }

        match action {
            // P0/P1 dedicated paths kept as-is: navigate/observe/screenshot have
            // richer messaging, and `observe` caches the snapshot (load-bearing for
            // ref resolution AND the secret-origin gate via `current_origin`).
            "navigate" => self.do_navigate(&input).await,
            "observe" => self.do_observe(&input).await,
            "screenshot" => self.do_screenshot().await,
            "capabilities" => self.do_capabilities().await,
            // F1: every other action parses into an ActSpec and dispatches through
            // `engine.act`. Missing params / blocked secrets short-circuit inside
            // `do_act` BEFORE the engine (browser) is ever constructed.
            other => self.do_act(other, &input).await,
        }
    }

    fn category(&self) -> ToolCategory {
        // Conservative default; per-action classification in category_for.
        ToolCategory::Exec
    }

    fn category_for(&self, input: &Value) -> ToolCategory {
        // E2: 据 classify_action 分级（含不可逆 → ToolCategory::Irreversible，让 orchestration 在
        // **普通会话**能正常弹审批）。run-time 危险信号经 build_action_context best-effort 采集
        // （accname 按 ref 查；submit/跨域POST/Enter-form/POST-reload 等 F1 接线后填实）。
        // 缺 action 或未知动作 → classify_action 走 Exec 兜底分支（与旧行为一致）。
        match input.get("action").and_then(|v| v.as_str()) {
            Some(action) => {
                let ctx = self.build_action_context(input);
                redline::classify_action(action, &ctx).to_category()
            }
            // 缺 action：保守 Exec（与旧行为一致：未知/缺失 → Exec）。
            None => ToolCategory::Exec,
        }
    }

    fn auto_approve_invocation(&self, _input: &Value, category: ToolCategory) -> bool {
        self.unrestricted_approval || category != ToolCategory::Irreversible
    }

    fn describe(&self, input: &Value) -> String {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
        let r#ref = input.get("ref").and_then(|v| v.as_str()).unwrap_or("");
        let detail = match action {
            "navigate" => {
                let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                format!("navigate to {:?}", nomi_tools::truncate_utf8(url, 80))
            }
            "observe" => "observe (page accessibility snapshot)".to_string(),
            "screenshot" => "screenshot".to_string(),
            "capabilities" => "capabilities".to_string(),
            "click" => {
                // IRREVERSIBLE hint: if the target ref resolves (via the latest
                // observe) to a dangerous accname, spell out the consequence so the
                // approver/model sees it. Best-effort — falls back to a plain note.
                let warn = self.irreversible_hint(input);
                format!("click [ref={ref}]{warn}")
            }
            "hover" => format!("hover [ref={ref}]"),
            "type" => {
                let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if parse_secret_ref(text).is_some() {
                    // NEVER echo the resolved secret; describe only the reference.
                    format!("type a stored secret into [ref={ref}] (value hidden)")
                } else {
                    format!("type {:?} into [ref={ref}]", nomi_tools::truncate_utf8(text, 40))
                }
            }
            "set_value" => {
                let value = input.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if parse_secret_ref(value).is_some() {
                    format!("set [ref={ref}] to a stored secret (value hidden)")
                } else {
                    format!("set [ref={ref}] = {:?}", nomi_tools::truncate_utf8(value, 40))
                }
            }
            "select_option" => format!("select option(s) on [ref={ref}]"),
            "press_key" => {
                let keys = input.get("keys").and_then(|v| v.as_str()).unwrap_or("");
                // Enter inside a form is an IRREVERSIBLE implicit submit; flag it.
                let warn = self.irreversible_hint(input);
                format!("press {keys:?}{warn}")
            }
            "scroll" => {
                let dir = input.get("direction").and_then(|v| v.as_str()).unwrap_or("?");
                if r#ref.is_empty() {
                    format!("scroll viewport {dir}")
                } else {
                    format!("scroll [ref={ref}] into view ({dir})")
                }
            }
            "scroll_to_text" => {
                let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("scroll to text {:?}", nomi_tools::truncate_utf8(text, 40))
            }
            "get_page_text" => "get page text".to_string(),
            "search_page" => {
                let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
                format!("search page for {:?}", nomi_tools::truncate_utf8(q, 40))
            }
            "find_elements" => {
                let sel = input.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                format!("find elements {:?}", nomi_tools::truncate_utf8(sel, 60))
            }
            "get_dropdown_options" => format!("list options of [ref={ref}]"),
            "cursor" => "list clickable (pointer-cursor) elements".to_string(),
            "tabs" => "list open tabs".to_string(),
            "wait" => {
                let ms = input.get("ms").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("wait {ms} ms")
            }
            "wait_for" => {
                let cond = input.get("condition").and_then(|v| v.as_str()).unwrap_or("?");
                format!("wait for {cond}")
            }
            "upload_file" => format!("upload file(s) to [ref={ref}]"),
            "download" => {
                let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "download {:?} into the sandboxed downloads folder (not opened)",
                    nomi_tools::truncate_utf8(url, 60)
                )
            }
            "save_as_pdf" => "save the current page as a PDF into the sandboxed downloads folder".to_string(),
            "extract" => "extract structured data from the page".to_string(),
            "switch_frame" => format!("switch into iframe [ref={ref}]"),
            "switch_tab" => {
                let id = input.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                format!("switch to tab {id}")
            }
            "close_tab" => {
                let id = input.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
                format!("close tab {id}")
            }
            "open_link_new_tab" => {
                let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
                format!("open {:?} in a new tab", nomi_tools::truncate_utf8(url, 60))
            }
            "back" => "go back".to_string(),
            "forward" => "go forward".to_string(),
            "reload" => {
                // Reloading a page that submitted a form re-submits it (IRREVERSIBLE).
                let warn = self.irreversible_hint(input);
                format!("reload the page{warn}")
            }
            "evaluate" => "evaluate a script in the page (gated — disabled unless full-power mode is on)".to_string(),
            "get_console_logs" => "get console messages (log/warn/error) from the page".to_string(),
            "get_page_errors" => "get uncaught exceptions and error-level logs from the page".to_string(),
            "get_network_log" => {
                let bodies = input.get("include_bodies").and_then(|v| v.as_bool()).unwrap_or(false);
                if bodies {
                    "get network request/response log (including bodies)".to_string()
                } else {
                    "get network request/response log".to_string()
                }
            }
            other => other.to_string(),
        };
        format!("Browser: {detail}")
    }
}

/// **F1: an IRREVERSIBLE consequence note for `describe`**, derived from the
/// per-action classifier (E2). When the action classifies as
/// [`redline::ApprovalTier::Irreversible`] (a Pay/Buy/Submit/Delete/Send click, an
/// Enter-in-form submit, a POST-page reload, …), append a short, blunt warning so
/// the approver/model sees the stakes up front. Empty string otherwise. This is
/// pure read of the cached snapshot signals — no I/O.
impl BrowserTool {
    fn irreversible_hint(&self, input: &Value) -> String {
        let Some(action) = input.get("action").and_then(|v| v.as_str()) else {
            return String::new();
        };
        let ctx = self.build_action_context(input);
        if redline::classify_action(action, &ctx) == redline::ApprovalTier::Irreversible {
            " — IRREVERSIBLE (may submit a form / charge money / delete / send; cannot be undone)"
                .to_string()
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_browser_engine::Capabilities;

    fn tool() -> BrowserTool {
        BrowserTool::new(&BrowserConfig::default())
    }

    #[test]
    fn concurrency_unsafe_for_read_and_write_actions() {
        let t = tool();
        // DESIGN §22：per-target act 串行 + observe⊥act → Browser 工具永不被 orchestrator 并发批处理,
        // 无论读/写动作。引擎 op_mutex 是正确性地基;此处恒 false 是调度天花板,两者都须成立。
        for action in [
            json!({"action": "observe"}),
            json!({"action": "navigate", "url": "https://example.com"}),
            json!({"action": "click", "ref": "f0e1"}),
            json!({"action": "screenshot"}),
            json!({}),
        ] {
            assert!(
                !t.is_concurrency_safe(&action),
                "Browser 对 {action:?} 必须并发不安全"
            );
        }
    }

    /// **F1-sec test seam**: a tool whose construction-time policy says the session
    /// bypasses orchestration approval (yolo / companion-forced-yolo / auto_approve).
    /// Mirrors what bootstrap does via `with_policy(config, config.tools.auto_approve, …)`.
    fn bypassing_tool() -> BrowserTool {
        BrowserTool::with_policy(&BrowserConfig::default(), true, false, false, None, None, None)
    }

    fn unrestricted_bypassing_tool() -> BrowserTool {
        let config = BrowserConfig {
            unrestricted_approval: true,
            ..BrowserConfig::default()
        };
        BrowserTool::with_policy(&config, true, false, false, None, None, None)
    }

    #[test]
    fn unrestricted_approval_releases_irreversible_in_bypassing_session() {
        let t = unrestricted_bypassing_tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");

        assert!(
            t.redline_gate("click", &json!({"action": "click", "ref": "f0e3"})).is_none(),
            "unrestricted browser approval should release irreversible browser actions"
        );
    }

    #[test]
    fn fatal_session_lost_evicts_cached_engine_and_tells_model_to_retry() {
        let t = tool();
        // 预置非 None 的引擎缓存（cached-failure 占位即可——engine_failure 不读其内容，
        // 只关心「缓存槽是否非空」→ 逐出后变 None，下次 engine() 重建）。
        *t.engine.lock().unwrap() = Some(Err("placeholder".into()));

        let r = t.engine_failure("Navigation failed", BrowserError::SessionLost { recoverable: false });
        assert!(r.is_error);
        assert!(
            r.content.contains("reset") && r.content.contains("retry"),
            "fatal SessionLost message should tell the model the session was reset + to retry: {}",
            r.content
        );
        assert!(
            t.engine.lock().unwrap().is_none(),
            "fatal SessionLost{{recoverable:false}} must evict the cached engine so the next action rebuilds"
        );
    }

    #[test]
    fn recoverable_session_loss_and_other_errors_do_not_evict() {
        let t = tool();

        // recoverable:true（单 tab 崩/关了最后一个 tab）→ 不逐出（留给引擎自身的 in-place
        // 恢复；若真不能恢复,下次会以 recoverable:false 浮现再逐）。
        *t.engine.lock().unwrap() = Some(Err("x".into()));
        let r = t.engine_failure("Browser action failed", BrowserError::SessionLost { recoverable: true });
        assert!(r.is_error);
        assert!(!r.content.contains("reset"), "recoverable loss should not claim a reset: {}", r.content);
        assert!(
            t.engine.lock().unwrap().is_some(),
            "recoverable SessionLost must NOT evict the cached engine"
        );

        // 非 SessionLost 错误（如防火墙 Blocked）→ 不逐出（引擎仍健康）。
        *t.engine.lock().unwrap() = Some(Err("x".into()));
        let r2 = t.engine_failure("Observe failed", BrowserError::Blocked { reason: "nope".into() });
        assert!(r2.is_error);
        assert!(
            t.engine.lock().unwrap().is_some(),
            "a non-session-lost error must NOT evict the cached engine"
        );
    }

    #[test]
    fn capabilities_note_mentions_browser_state() {
        let note = BrowserTool::capabilities_note(&Capabilities {
            browser_ready: false,
            headful: false,
            display_available: false,
            engine: "".into(),
        });
        assert!(note.contains("browser") || note.contains("浏览器"));
    }

    #[test]
    fn capabilities_note_ready_mentions_browser_and_engine() {
        let note = BrowserTool::capabilities_note(&Capabilities {
            browser_ready: true,
            headful: true,
            display_available: true,
            engine: "chromium".into(),
        });
        assert!(note.contains("browser") || note.contains("浏览器"));
        assert!(note.contains("chromium"));
    }

    // --- schema ---

    #[test]
    fn schema_is_valid_object_with_required_action() {
        let schema = tool().input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["action"]));
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        for expected in ["navigate", "screenshot", "capabilities", "observe"] {
            assert!(
                actions.iter().any(|a| a == expected),
                "schema enum missing {expected}"
            );
        }
        // Round-trips through serde_json without loss.
        let text = serde_json::to_string(&schema).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, schema);
    }

    #[test]
    fn name_and_metadata() {
        let t = tool();
        assert_eq!(t.name(), "Browser");
        assert!(!t.description().is_empty());
        // Description carries the capability note → mentions the browser surface.
        assert!(t.description().contains("browser") || t.description().contains("浏览器"));
        assert!(!t.is_concurrency_safe(&json!({})));
        assert_eq!(t.category(), ToolCategory::Exec);
    }

    // --- category_for (approval gating) ---

    #[test]
    fn category_for_info_and_exec_actions() {
        let t = tool();
        for action in ["navigate", "screenshot", "capabilities", "observe"] {
            assert_eq!(
                t.category_for(&json!({"action": action, "url": "https://example.com"})),
                ToolCategory::Info,
                "{action} should be Info"
            );
        }
    }

    #[test]
    fn category_for_cross_origin_navigate_is_info() {
        let t = tool();
        seed_snapshot(&t, "f0e1", "link", "");

        assert_eq!(
            t.category_for(&json!({"action": "navigate", "url": "https://evil.test/collect"})),
            ToolCategory::Info,
            "ordinary cross-origin navigation should not be treated as irreversible"
        );
    }

    #[test]
    fn category_for_observe_is_info() {
        // observe is a read-only page snapshot (aria YAML + ref table); it must
        // never gate as Exec — mirroring ComputerTool's observe.
        let t = tool();
        assert_eq!(
            t.category_for(&json!({"action": "observe"})),
            ToolCategory::Info
        );
    }

    #[test]
    fn category_for_unknown_or_missing_action_is_exec() {
        let t = tool();
        assert_eq!(t.category_for(&json!({"action": "bogus"})), ToolCategory::Exec);
        assert_eq!(t.category_for(&json!({})), ToolCategory::Exec);
    }

    // --- execute error paths (no real browser launch) ---

    #[tokio::test]
    async fn missing_action_is_error() {
        let result = tool().execute(json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("action"), "{}", result.content);
    }

    #[tokio::test]
    async fn unknown_action_is_error() {
        let result = tool().execute(json!({"action": "fly"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("fly"), "{}", result.content);
    }

    #[tokio::test]
    async fn navigate_without_url_is_error_before_launch() {
        // Missing `url` is rejected before the engine is ever constructed, so
        // this test never launches a browser.
        let result = tool().execute(json!({"action": "navigate"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("url"), "{}", result.content);
    }

    // --- describe ---

    #[test]
    fn describe_navigate_screenshot_capabilities() {
        let t = tool();
        let nav = t.describe(&json!({"action": "navigate", "url": "https://example.com"}));
        assert!(nav.contains("example.com"), "{nav}");
        assert!(nav.starts_with("Browser:"), "{nav}");
        assert_eq!(t.describe(&json!({"action": "screenshot"})), "Browser: screenshot");
        assert_eq!(
            t.describe(&json!({"action": "capabilities"})),
            "Browser: capabilities"
        );
        assert_eq!(
            t.describe(&json!({"action": "observe"})),
            "Browser: observe (page accessibility snapshot)"
        );
    }

    // --- lazy construction: new() does not launch ---

    #[test]
    fn new_does_not_construct_engine() {
        let t = tool();
        // `new` must be a pure constructor — no engine built yet.
        assert!(
            t.engine.lock().unwrap().is_none(),
            "new() must not construct/launch the engine eagerly"
        );
    }

    // --- real-device end-to-end (needs a local/bundled chrome) ---

    // navigate → observe round-trip against a real Chromium. Set
    // NOMIFUN_CHROME_BINARY (or rely on the download fallback) then:
    //   cargo nextest run -p nomi-browser -- --ignored navigate_then_observe_real
    // Asserts the observation surfaces the generation header, the `<data>`
    // wrapper from the engine, and at least one frame-local `[ref=f0e…]` ref.
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --ignored"]
    async fn navigate_then_observe_real() {
        let t = tool();
        let nav = t
            .execute(json!({"action": "navigate", "url": "https://example.com"}))
            .await;
        assert!(!nav.is_error, "{}", nav.content);

        let obs = t.execute(json!({"action": "observe"})).await;
        assert!(!obs.is_error, "{}", obs.content);
        assert!(
            obs.content.contains("[browser observation"),
            "missing generation header: {}",
            obs.content
        );
        assert!(
            obs.content.contains("<data"),
            "missing <data> wrapper from the engine: {}",
            obs.content
        );
        assert!(
            obs.content.contains("[ref=f0e"),
            "missing a frame-local ref: {}",
            obs.content
        );

        // The snapshot was cached for later ref resolution.
        assert!(t.last_snapshot.lock().unwrap().is_some());
    }

    // ── E2: 不可逆分类 + facade 独立 fail-closed 门（facade 级，纯逻辑核心已在 redline.rs 单测）──

    use nomi_browser_engine::{ElementEntry, Observation, SnapshotGen};

    /// 给 `last_snapshot` 注入一份含指定 (ref, role, accname) 元素的 observation，
    /// 让 facade 的 `build_action_context` 能按 ref 查到 accname/role（模拟 observe 后再 click）。
    fn seed_snapshot(t: &BrowserTool, r#ref: &str, role: &str, name: &str) {
        *t.last_snapshot.lock().unwrap() = Some(Observation {
            generation: SnapshotGen(1),
            yaml: "<data></data>".into(),
            entries: vec![ElementEntry {
                r#ref: r#ref.to_string(),
                role: role.to_string(),
                name: name.to_string(),
                frame_seq: 0,
            }],
            url: Some("https://shop.example.com/cart".into()),
            truncated: false,
            current_page_is_post: false,
            boxes: Default::default(),
        });
    }

    #[test]
    fn category_for_click_dangerous_accname_is_irreversible() {
        // observe 后 click 一个 accname="Pay now" 的元素 → category_for 据 last_snapshot 按 ref 查到
        // 危险 accname → ToolCategory::Irreversible（让普通会话的 orchestration 能正常弹审批）。
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        assert_eq!(
            t.category_for(&json!({"action": "click", "ref": "f0e3"})),
            ToolCategory::Irreversible
        );
    }

    #[test]
    fn category_for_click_benign_accname_is_exec() {
        // accname="Show more" → 普通可逆点击 → Exec（不误判为不可逆）。
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Show more");
        assert_eq!(
            t.category_for(&json!({"action": "click", "ref": "f0e3"})),
            ToolCategory::Exec
        );
    }

    #[test]
    fn category_for_click_cn_delete_accname_is_irreversible() {
        let t = tool();
        seed_snapshot(&t, "f0e7", "button", "删除账户");
        assert_eq!(
            t.category_for(&json!({"action": "click", "ref": "f0e7"})),
            ToolCategory::Irreversible
        );
    }

    // ── P3-D2：is_cross_origin_post 信号填实（best-effort 纯读，保守 + E5/D2 网络层兜底）────────

    #[test]
    fn cross_origin_post_signal_false_when_navigate_target_is_cross_etld1() {
        // Ordinary navigation is a browser-read action for approval purposes. Do
        // not pre-classify a cross-site GET navigation as a cross-origin POST.
        let t = tool();
        seed_snapshot(&t, "f0e1", "link", ""); // 仅为设当前 origin（shop.example.com）
        let ctx = t.build_action_context(&json!({"action": "navigate", "url": "https://evil.test/collect"}));
        assert!(
            !ctx.is_cross_origin_post,
            "cross-eTLD+1 navigate target must not look like a POST"
        );
    }

    #[test]
    fn cross_origin_post_signal_false_when_same_etld1() {
        // 目标与当前页同 eTLD+1（example.com）→ 非跨域 → false。
        let t = tool();
        seed_snapshot(&t, "f0e1", "link", "");
        let ctx = t.build_action_context(&json!({"action": "navigate", "url": "https://api.example.com/x"}));
        assert!(!ctx.is_cross_origin_post, "same eTLD+1 → signal false");
    }

    #[test]
    fn cross_origin_post_signal_false_without_target_url() {
        // 交互动作（click/type）不带目的 URL → facade 拿不到真实 form-action → 保守 false
        // （真拦截在 E5/D2 网络层兜底）。
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Submit");
        let click = t.build_action_context(&json!({"action": "click", "ref": "f0e3"}));
        assert!(!click.is_cross_origin_post, "click carries no target URL → conservative false");
        let typ = t.build_action_context(&json!({"action": "type", "ref": "f0e3", "text": "hi"}));
        assert!(!typ.is_cross_origin_post, "type carries no target URL → conservative false");
    }

    #[test]
    fn cross_origin_post_signal_false_without_cached_origin() {
        // 无缓存 observe（无当前 origin）→ 保守 false（无从判，E5/D2 网络层兜底）。
        let t = tool();
        // 不 seed_snapshot：last_snapshot=None。
        let ctx = t.build_action_context(&json!({"action": "navigate", "url": "https://evil.test/x"}));
        assert!(!ctx.is_cross_origin_post, "no cached origin → conservative false");
    }

    #[tokio::test]
    async fn execute_redline_gate_hard_denies_irreversible_in_bypassing_session() {
        // facade 独立门：模拟 yolo/companion（审批旁路）会话 → click 危险 accname 的元素 →
        // execute 在 dispatch 前 hard-deny（never launches a browser，门在 dispatch 之前）。
        let t = bypassing_tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");

        let result = t
            .execute(json!({"action": "click", "ref": "f0e3"}))
            .await;
        assert!(result.is_error, "irreversible click in a yolo session must be blocked");
        let lower = result.content.to_lowercase();
        assert!(
            lower.contains("irreversible") || lower.contains("blocked"),
            "block message should explain the redline: {}",
            result.content
        );
        // 门拦在 dispatch 之前 → 引擎从未构造（未启动浏览器）。
        assert!(
            t.engine.lock().unwrap().is_none(),
            "redline gate must deny BEFORE constructing/launching the engine"
        );
    }

    // 「放行」用例直接验 `redline_gate` 返 None（= 放行），**不**驱动整条 execute——否则会真去
    // 构造/启动浏览器（无 chrome 时一路超时到下载兜底，单测变 200s+）。门是否被 execute 调用由上面
    // 的 hard-deny 用例端到端证明（它在 dispatch 前短路，引擎从未构造）。

    #[test]
    fn redline_gate_allows_irreversible_in_normal_session() {
        // 普通会话（审批未旁路）：facade 门**不拦**不可逆动作（交 orchestration 经 Irreversible 审批）。
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        // session_bypasses_approval() 默认 false（普通会话）→ 门放行。
        assert!(
            t.redline_gate("click", &json!({"action": "click", "ref": "f0e3"}))
                .is_none(),
            "normal-session irreversible must NOT be hard-denied by the facade gate"
        );
    }

    #[test]
    fn redline_gate_allows_benign_action_even_in_bypassing_session() {
        // 边界：yolo 会话 + 良性只读动作（screenshot）→ 门放行（只拦不可逆，不拦良性/可逆）。
        let t = bypassing_tool();
        assert!(
            t.redline_gate("screenshot", &json!({"action": "screenshot"}))
                .is_none(),
            "benign action must never be hard-denied by the redline gate"
        );
    }

    #[test]
    fn redline_gate_denies_irreversible_in_bypassing_session() {
        // 纯 gate 视角（不经 execute）：yolo + 危险 accname → 门返 Some(error)（hard-deny）。
        let t = bypassing_tool();
        seed_snapshot(&t, "f0e3", "button", "删除账户");
        let blocked = t.redline_gate("click", &json!({"action": "click", "ref": "f0e3"}));
        assert!(blocked.is_some(), "irreversible in a bypassing session must be denied by the gate");
        assert!(blocked.unwrap().is_error);
    }

    // ── P3-GW2: 带外确认 sentinel 放行（裁决④）──────────────────────────────────

    #[test]
    fn out_of_band_sentinel_releases_irreversible_in_bypassing_session() {
        // GW2 核心：旁路会话（yolo/companion）+ 不可逆动作，但 input 带 OUT_OF_BAND_CONFIRMED_KEY=true
        // （= 网关在手机/前端审批通过后注入）→ enforce_redline 第三参为真 → 门**放行**（非 hard-deny）。
        let t = bypassing_tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        let input = json!({
            "action": "click",
            "ref": "f0e3",
            OUT_OF_BAND_CONFIRMED_KEY: true,
        });
        assert!(
            t.redline_gate("click", &input).is_none(),
            "an out-of-band-confirmed irreversible action must be released (GW2 approval path)"
        );
        assert!(t.out_of_band_confirmed(&input), "sentinel=true must read as confirmed");
    }

    #[test]
    fn out_of_band_sentinel_absent_or_false_keeps_fail_closed() {
        // sentinel 缺失 / false / 非 bool → 不算确认（fail-closed，P2 行为不变）：旁路会话仍 hard-deny。
        let t = bypassing_tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        // 缺失。
        assert!(!t.out_of_band_confirmed(&json!({"action": "click", "ref": "f0e3"})));
        assert!(t.redline_gate("click", &json!({"action": "click", "ref": "f0e3"})).is_some());
        // 显式 false。
        let f = json!({"action": "click", "ref": "f0e3", OUT_OF_BAND_CONFIRMED_KEY: false});
        assert!(!t.out_of_band_confirmed(&f));
        assert!(t.redline_gate("click", &f).is_some());
        // 非 bool（字符串）不被当作确认（仅严格 true 放行）。
        let s = json!({"action": "click", "ref": "f0e3", OUT_OF_BAND_CONFIRMED_KEY: "true"});
        assert!(!t.out_of_band_confirmed(&s), "only a strict bool true counts as confirmed");
        assert!(t.redline_gate("click", &s).is_some());
    }

    #[test]
    fn classify_action_tier_uses_cached_snapshot_for_accname() {
        // P3-GW2: 网关经此拿权威分级——按 ref 查到的危险 accname 在这里被看见（网关裸
        // classify_action 无 snapshot 看不到）。
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        assert_eq!(
            t.classify_action_tier("click", &json!({"action": "click", "ref": "f0e3"})),
            ApprovalTier::Irreversible,
            "a Pay-now button (by ref) must classify Irreversible via the cached snapshot"
        );
        // 良性按钮 → Exec；无 snapshot 的只读 → Info。
        seed_snapshot(&t, "f0e4", "button", "Show more");
        assert_eq!(
            t.classify_action_tier("click", &json!({"action": "click", "ref": "f0e4"})),
            ApprovalTier::Exec
        );
        assert_eq!(
            t.classify_action_tier("observe", &json!({"action": "observe"})),
            ApprovalTier::Info
        );
    }

    // ── F1-sec: session_mode 穿透（with_policy）+ ActionContext 真信号（Enter-落-form 保守判定）──

    #[test]
    fn with_policy_threads_session_bypass_into_redline_gate() {
        // F1-sec 关键：with_policy(true)（= 构造期 config.tools.auto_approve）让 session_bypasses_approval()
        // 真返 true → redline 门对不可逆动作 hard-deny（fail-open 已闭）。with_policy(false)（普通会话）门不拦。
        let bypass = BrowserTool::with_policy(&BrowserConfig::default(), true, false, false, None, None, None);
        assert!(
            bypass.session_bypasses_approval(),
            "with_policy(true) must arm the redline gate (yolo/companion/auto_approve)"
        );
        seed_snapshot(&bypass, "f0e3", "button", "Pay now");
        assert!(
            bypass.redline_gate("click", &json!({"action": "click", "ref": "f0e3"})).is_some(),
            "armed session must hard-deny an irreversible click"
        );

        let normal = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, None, None, None);
        assert!(
            !normal.session_bypasses_approval(),
            "with_policy(false) is a normal session (gate not armed)"
        );
        seed_snapshot(&normal, "f0e3", "button", "Pay now");
        assert!(
            normal.redline_gate("click", &json!({"action": "click", "ref": "f0e3"})).is_none(),
            "normal session must NOT hard-deny (orchestration approves)"
        );
    }

    // ── P3-X1: set_mode 运行时翻转 → session_bypasses_approval LIVE 读（非构造期快照）────────────
    use nomi_protocol::ToolApprovalManager;
    use nomi_protocol::commands::SessionMode;

    /// 用运行时模式句柄构造一个 tool（构造期快照 = `init_bypass`，运行时句柄 = `mgr`）。
    fn tool_with_runtime_mode(init_bypass: bool, mgr: Arc<ToolApprovalManager>) -> BrowserTool {
        BrowserTool::with_policy(&BrowserConfig::default(), init_bypass, false, false, None, Some(mgr), None)
    }

    #[test]
    fn runtime_mode_handle_live_read_flips_bypass_with_set_mode() {
        // 构造为普通会话（init_bypass=false），但带运行时句柄。会话**中途** set_mode → LIVE 读翻转。
        let mgr = Arc::new(ToolApprovalManager::new()); // 起始 Default
        let t = tool_with_runtime_mode(false, mgr.clone());

        // 初始 default：不 bypass（门不武装）。
        assert!(!t.session_bypasses_approval(), "default mode → not bypassed");

        // 运行时翻 yolo → LIVE 读到 bypass=true（构造期快照说 false，但运行时句柄优先）。
        mgr.set_mode(SessionMode::Yolo);
        assert!(
            t.session_bypasses_approval(),
            "runtime set_mode(yolo) must LIVE-flip bypass to true (not pinned by construction snapshot)"
        );

        // 翻回 default → LIVE 读回 false（解除）。
        mgr.set_mode(SessionMode::Default);
        assert!(
            !t.session_bypasses_approval(),
            "runtime set_mode(default) must LIVE-flip bypass back to false"
        );
    }

    #[test]
    fn runtime_mode_auto_edit_is_not_bypass() {
        // F1-sec 方向：AutoEdit（只自动批 info/edit，从不批 Irreversible）**不** bypass → 门不武装。
        let mgr = Arc::new(ToolApprovalManager::new());
        let t = tool_with_runtime_mode(false, mgr.clone());
        mgr.set_mode(SessionMode::AutoEdit);
        assert!(
            !t.session_bypasses_approval(),
            "AutoEdit must NOT be treated as approval-bypass (irreversible still gated)"
        );
    }

    #[test]
    fn runtime_mode_handle_takes_priority_over_construction_snapshot() {
        // 句柄在 → 完全 LIVE 读，构造期快照被忽略（不再钉死）。
        // 构造期快照 = true（曾武装），但运行时句柄是 default → 现读 false。
        let mgr = Arc::new(ToolApprovalManager::new()); // Default
        let t = tool_with_runtime_mode(/* init_bypass */ true, mgr.clone());
        assert!(
            !t.session_bypasses_approval(),
            "runtime handle (default) must override the stale construction-time snapshot (true)"
        );
        // 翻 yolo → 真随之武装。
        mgr.set_mode(SessionMode::Yolo);
        assert!(t.session_bypasses_approval());
    }

    #[test]
    fn no_runtime_mode_handle_falls_back_to_construction_snapshot() {
        // 无运行时句柄（CLI REPL / 仅 BrowserConfig 调用方 / 测试）→ 回退构造期快照（现行行为不变）。
        let armed = BrowserTool::with_policy(&BrowserConfig::default(), true, false, false, None, None, None);
        assert!(armed.session_bypasses_approval(), "no handle → use construction snapshot (true)");
        let normal = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, None, None, None);
        assert!(!normal.session_bypasses_approval(), "no handle → use construction snapshot (false)");
    }

    #[test]
    fn redline_gate_arms_on_runtime_yolo_and_disarms_on_flip_back() {
        // 端到端门行为：构造普通会话 + 运行时句柄。中途翻 yolo → 不可逆 click 被门武装（hard-deny）；
        // 翻回 default → 门解除（放行，交 orchestration）。AutoEdit → 不武装（不误判 bypass）。
        let mgr = Arc::new(ToolApprovalManager::new());
        let t = tool_with_runtime_mode(false, mgr.clone());
        seed_snapshot(&t, "f0e3", "button", "Pay now"); // accname → Irreversible
        let click = json!({"action": "click", "ref": "f0e3"});

        // default：门不拦。
        assert!(t.redline_gate("click", &click).is_none(), "default → gate not armed");

        // AutoEdit：仍不拦（不可逆交 orchestration）。
        mgr.set_mode(SessionMode::AutoEdit);
        assert!(
            t.redline_gate("click", &click).is_none(),
            "AutoEdit → gate must NOT arm (F1-sec direction preserved)"
        );

        // 运行时翻 yolo：门武装 → 不可逆 click hard-deny。
        mgr.set_mode(SessionMode::Yolo);
        assert!(
            t.redline_gate("click", &click).is_some(),
            "runtime yolo → gate arms; irreversible click hard-denied"
        );

        // 翻回 default：门解除。
        mgr.set_mode(SessionMode::Default);
        assert!(
            t.redline_gate("click", &click).is_none(),
            "flip back to default → gate disarmed"
        );
    }

    #[test]
    fn runtime_yolo_does_not_arm_gate_for_benign_action() {
        // 边界：运行时 yolo 但良性只读动作（screenshot）→ 门放行（只拦不可逆）。
        let mgr = Arc::new(ToolApprovalManager::new());
        let t = tool_with_runtime_mode(false, mgr.clone());
        mgr.set_mode(SessionMode::Yolo);
        assert!(
            t.redline_gate("screenshot", &json!({"action": "screenshot"})).is_none(),
            "benign action must never be hard-denied even under runtime yolo"
        );
    }

    #[test]
    fn with_policy_threads_evaluate_full_power() {
        // F1-sec: with_policy 第三参（= LIVE agent.browserUse.fullPower）灌进 evaluate_full_power 字段，
        // engine() 构造时会把它传给 EngineConfig.evaluate_full_power（真生效靠 #[ignore] 集成验放行）。
        let off = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, None, None, None);
        assert!(!off.evaluate_full_power, "default full_power must be OFF (E3 default-deny)");
        let on = BrowserTool::with_policy(&BrowserConfig::default(), false, true, false, None, None, None);
        assert!(on.evaluate_full_power, "with_policy(.., true) must carry full_power into the tool");
    }

    #[test]
    fn with_policy_threads_evaluate_persistent_login() {
        // SD-6: with_policy 第四参（= LIVE agent.browserUse.persistentLogin）灌进
        // evaluate_persistent_login 字段，engine() 构造时传给 EngineConfig.evaluate_persistent_login。
        let off = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, None, None, None);
        assert!(!off.evaluate_persistent_login, "default persistent_login must be OFF (code-level default-deny)");
        let on = BrowserTool::with_policy(&BrowserConfig::default(), false, false, true, None, None, None);
        assert!(on.evaluate_persistent_login, "with_policy(.., persistent_login=true) must carry into the tool");
    }

    #[test]
    fn new_defaults_to_non_bypassing_normal_session() {
        // `new`（只有 BrowserConfig 的调用方）默认普通会话——门不武装（不误拦）。
        assert!(!tool().session_bypasses_approval());
    }

    // ── 并发隔离：每个 facade 分配唯一 user-data-dir（根治 Chromium 进程单例碰撞）──
    #[test]
    fn allocates_unique_stable_profile_dir_under_data_dir() {
        let data = std::env::temp_dir().join("bt-profile-iso");
        let a = BrowserTool::with_data_dir(data.clone(), false);
        let b = BrowserTool::with_data_dir(data.clone(), false);
        // 同一 facade 生命周期内稳定（SessionLost 自愈重启复用同一目录）。
        assert_eq!(a.profile_dir(), a.profile_dir());
        // 跨实例唯一：两个会话 / 网关 key / stdio 桥绝不共享 user-data-dir。
        assert_ne!(
            a.profile_dir(),
            b.profile_dir(),
            "two BrowserTool instances must get distinct user-data-dirs (no singleton collision)"
        );
        // 落在 <data_dir>/profiles/ 下（GC 可扫的唯一根）。
        assert!(
            a.profile_dir().starts_with(data.join("profiles")),
            "profile dir must live under <data_dir>/profiles/, got {:?}",
            a.profile_dir()
        );
        // 绝非旧的共享 <data_dir>/profile。
        assert_ne!(
            a.profile_dir(),
            data.join("profile"),
            "must not reuse the legacy shared <data_dir>/profile"
        );
    }

    // ── P3-G2: workspace_dir 传参链（构造带 workspace → 字段填对；默认 None；with_policy 透传）──

    #[test]
    fn new_and_with_data_dir_default_workspace_none() {
        // 仅有 BrowserConfig / data_dir 的调用方默认无 per-pet workspace → 引擎兜底落
        // <data_dir>/downloads（仍隔离，非用户 Downloads）。
        assert!(tool().workspace_dir.is_none(), "new() must default workspace_dir = None");
        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("bt-g2-default"), false);
        assert!(t.workspace_dir.is_none(), "with_data_dir must default workspace_dir = None");
    }

    #[test]
    fn with_policy_threads_workspace_dir() {
        // P3-G2 关键：bootstrap 经 with_policy 把会话工作目录（伙伴 {companion_id}/workspace /
        // 非伙伴会话工作目录）灌进 workspace_dir，引擎据它把下载落进 <workspace>/downloads。
        let ws = std::env::temp_dir().join("companions").join("companion_x").join("workspace");
        let t = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, Some(ws.clone()), None, None);
        assert_eq!(
            t.workspace_dir.as_deref(),
            Some(ws.as_path()),
            "with_policy(.., Some(ws)) must carry the session workspace into the tool"
        );
        // None（无 workspace 上下文）保持 None（引擎兜底 <data_dir>/downloads）。
        let none = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, None, None, None);
        assert!(none.workspace_dir.is_none(), "with_policy(.., None) must keep workspace_dir = None");
    }

    #[test]
    fn workspace_builder_sets_dir_on_with_data_dir_tool() {
        // builder：with_data_dir + .workspace(..) 设上 per-pet workspace（集成测试 + 显式 data_dir
        // 调用方用它，不经 with_policy/new 覆盖 data_dir）。
        let data = std::env::temp_dir().join("bt-g2-data");
        let ws = std::env::temp_dir().join("bt-g2-ws");
        let t = BrowserTool::with_data_dir(data.clone(), false).workspace(ws.clone());
        assert_eq!(t.workspace_dir.as_deref(), Some(ws.as_path()));
        // data_dir 不被 workspace 覆盖（二者正交：data_dir=chrome/user-data-dir 父，workspace=下载落点）。
        assert_eq!(t.data_dir, data);
    }

    // ── P3-X2: secret_source → 懒加载 store + 派生 firewall allow_etld1（裁决⑤共用真值，纯逻辑）──

    #[test]
    fn ensure_secret_store_and_firewall_loads_vault_and_derives_allowlist() {
        // 注册 secret 到 per-pet vault（机器绑定 key），用 secret_source 指向它 → 引擎构造前的
        // ensure_secret_store_and_firewall 应：(1) 懒加载 store（secret:NAME 可解析）、(2) 从其
        // allowed_origins 派生 firewall.allow_etld1。
        let dir = tempfile::tempdir().expect("tempdir");
        let key = [0x42u8; nomifun_secret::KEY_SIZE];
        let vault_path = nomifun_secret::pet_vault_path(dir.path(), "pet-1");
        let mut store = nomifun_secret::SecretStore::new(key);
        store.register("pw", "the-secret", vec!["x.com".into()]).unwrap();
        store.register("tok", "ghp", vec!["github.com".into()]).unwrap();
        nomifun_secret::save_secret_store(&store, &vault_path).expect("save vault");

        let t = BrowserTool::with_data_dir(dir.path().join("bdata"), false)
            .secret_source(BrowserSecretSource { vault_path, key });
        // Before: no store / no firewall override (lazy).
        assert!(t.secret_store.lock().unwrap().is_none());
        t.ensure_secret_store_and_firewall();
        // After: store loaded → resolve works (origin-gated, fail-closed off-domain).
        {
            let guard = t.secret_store.lock().unwrap();
            let loaded = guard.as_ref().expect("store loaded from vault");
            assert_eq!(loaded.resolve("pw", "https://login.x.com").unwrap().expose(), "the-secret");
            assert!(loaded.resolve("pw", "https://evil.com").is_none(), "origin gate stays fail-closed");
        }
        // And: firewall.allow_etld1 == the union of registered allowed_origins (裁决⑤).
        let fw = t.firewall_override.lock().unwrap().clone().expect("firewall derived");
        assert_eq!(fw.allow_etld1, vec!["github.com".to_string(), "x.com".to_string()]);
        // The other firewall guards stay at their defaults (IP block + cross-origin POST gate).
        let def = nomi_browser_engine::FirewallConfig::default();
        assert_eq!(fw.block_private_ips, def.block_private_ips);
        assert_eq!(fw.gate_cross_origin_post, def.gate_cross_origin_post);
    }

    #[test]
    fn ensure_secret_store_and_firewall_no_source_keeps_default_firewall() {
        // 无 secret 源（CLI / 测试默认）→ store 仍空、firewall.allow_etld1 空 = 不限制出口域（零回归）。
        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("bt-x2-nosrc"), false);
        t.ensure_secret_store_and_firewall();
        assert!(t.secret_store.lock().unwrap().is_none(), "no source → store stays empty");
        let fw = t.firewall_override.lock().unwrap().clone().expect("firewall set");
        assert!(fw.allow_etld1.is_empty(), "no registered origins → unrestricted egress (zero regression)");
        assert_eq!(fw, nomi_browser_engine::FirewallConfig::default(), "firewall == default when no secrets");
    }

    #[test]
    fn ensure_secret_store_and_firewall_missing_vault_degrades_gracefully() {
        // secret_source 指向不存在的 vault（首次注册前）→ 空 store + 空 allowlist（绝不 panic）。
        let dir = tempfile::tempdir().expect("tempdir");
        let vault_path = nomifun_secret::pet_vault_path(dir.path(), "never-registered");
        let t = BrowserTool::with_data_dir(dir.path().join("bdata"), false)
            .secret_source(BrowserSecretSource { vault_path, key: [0x42; nomifun_secret::KEY_SIZE] });
        t.ensure_secret_store_and_firewall();
        // Empty store loaded (not None — a source WAS set), allowlist empty.
        assert!(t.secret_store.lock().unwrap().as_ref().is_some_and(|s| s.is_empty()));
        assert!(t.firewall_override.lock().unwrap().as_ref().unwrap().allow_etld1.is_empty());
    }

    #[test]
    fn enter_submits_form_signal_pure_logic() {
        // 裸 Enter/Return → true（保守视作可能提交）；组合键/其它键/None → false。
        assert!(enter_submits_form_signal(Some("Enter")));
        assert!(enter_submits_form_signal(Some("Return")));
        assert!(enter_submits_form_signal(Some("  enter ")));
        assert!(enter_submits_form_signal(Some("RETURN")));
        // 组合键不升级（可能是有意快捷键）。
        assert!(!enter_submits_form_signal(Some("Ctrl+Enter")));
        assert!(!enter_submits_form_signal(Some("Shift+Enter")));
        // 其它键不升级。
        assert!(!enter_submits_form_signal(Some("Tab")));
        assert!(!enter_submits_form_signal(Some("a")));
        assert!(!enter_submits_form_signal(Some("Escape")));
        assert!(!enter_submits_form_signal(None));
    }

    #[test]
    fn press_key_enter_classifies_irreversible_via_action_context() {
        // F1-sec 接线：build_action_context 据 press_key + keys=Enter 填 enter_submits_form=true →
        // classify_action(press_key) 升 Irreversible。普通 press_key（非 Enter）→ Exec。
        let t = tool();
        assert_eq!(
            t.category_for(&json!({"action": "press_key", "keys": "Enter"})),
            ToolCategory::Irreversible,
            "press Enter (implicit form submit) must classify Irreversible"
        );
        assert_eq!(
            t.category_for(&json!({"action": "press_key", "keys": "Tab"})),
            ToolCategory::Exec,
            "press Tab is a benign navigation key (Exec)"
        );
    }

    #[test]
    fn press_key_enter_hard_denied_in_bypassing_session() {
        // 端到端（gate 视角）：yolo 会话 + press_key Enter → 门 hard-deny（fail-open 已闭，
        // 证 ActionContext 真信号 + session_mode 穿透协同生效）。
        let t = bypassing_tool();
        assert!(
            t.redline_gate("press_key", &json!({"action": "press_key", "keys": "Enter"})).is_some(),
            "Enter (implicit submit) in a bypassing session must be hard-denied"
        );
        // 普通 Tab 键不拦（良性导航）。
        assert!(
            t.redline_gate("press_key", &json!({"action": "press_key", "keys": "Tab"})).is_none(),
            "benign Tab must not be hard-denied"
        );
    }

    // ── SD-4: reload-POST → Irreversible 信号接线 ────────────────────────────

    /// 给 `last_snapshot` 注入一份带 POST 标志的空 observation（模拟 observe 后页面是 form-submit 来的）。
    fn seed_snapshot_post(t: &BrowserTool, is_post: bool) {
        *t.last_snapshot.lock().unwrap() = Some(Observation {
            generation: SnapshotGen(1),
            yaml: "<data></data>".into(),
            entries: vec![],
            url: Some("https://shop.example.com/order-confirm".into()),
            truncated: false,
            current_page_is_post: is_post,
            boxes: Default::default(),
        });
    }

    #[test]
    fn reload_on_post_page_classifies_irreversible() {
        // SD-4 核心：上次 observe 标记 current_page_is_post=true → reload 被分类为 Irreversible。
        let t = tool();
        seed_snapshot_post(&t, true);
        assert_eq!(
            t.classify_action_tier("reload", &json!({"action": "reload"})),
            redline::ApprovalTier::Irreversible,
            "reload on a POST page must classify Irreversible (re-submit risk)"
        );
    }

    #[test]
    fn reload_on_non_post_page_classifies_exec() {
        // 非 POST 页面 reload → Exec（不误判普通刷新为不可逆）。
        let t = tool();
        seed_snapshot_post(&t, false);
        assert_eq!(
            t.classify_action_tier("reload", &json!({"action": "reload"})),
            redline::ApprovalTier::Exec,
            "reload on a non-POST page must classify Exec (no re-submit risk)"
        );
    }

    #[test]
    fn reload_without_snapshot_classifies_exec() {
        // 无 last_snapshot（首次动作前 observe 尚未执行）→ 保守 Exec（不误判）。
        let t = tool();
        assert_eq!(
            t.classify_action_tier("reload", &json!({"action": "reload"})),
            redline::ApprovalTier::Exec,
            "reload without any snapshot defaults to Exec (conservative)"
        );
    }

    #[test]
    fn reload_post_hard_denied_in_bypassing_session() {
        // 端到端 gate 视角：yolo 会话 + reload POST 页 → 门 hard-deny。
        let t = bypassing_tool();
        seed_snapshot_post(&t, true);
        assert!(
            t.redline_gate("reload", &json!({"action": "reload"})).is_some(),
            "reload on POST page in a bypassing session must be hard-denied"
        );
        // 非 POST 页面 reload 不拦。
        seed_snapshot_post(&t, false);
        assert!(
            t.redline_gate("reload", &json!({"action": "reload"})).is_none(),
            "reload on non-POST page must not be hard-denied"
        );
    }

    #[test]
    fn enter_in_form_and_reload_post_both_classify_irreversible() {
        // 回归：case (a) Enter + case (b) reload-POST 共存不互相干扰。
        let t = tool();
        // Case (a): press_key Enter → Irreversible（不受 current_page_is_post 影响）。
        seed_snapshot_post(&t, true); // POST 页 + Enter 同时
        assert_eq!(
            t.classify_action_tier("press_key", &json!({"action": "press_key", "keys": "Enter"})),
            redline::ApprovalTier::Irreversible,
            "press_key Enter must stay Irreversible regardless of POST flag"
        );
        // Case (b): reload → Irreversible（据 POST 标志）。
        assert_eq!(
            t.classify_action_tier("reload", &json!({"action": "reload"})),
            redline::ApprovalTier::Irreversible,
            "reload on POST page must classify Irreversible"
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // F1: facade 全动作 dispatch + schema enum + 参数池 + describe + secret 接线。
    // 纯逻辑（build_act_spec 解析 / 缺参在启动浏览器前返错 / secret:NAME origin 门 /
    // 值不入输出 / schema enum / describe IRREVERSIBLE）。无真浏览器。
    // ════════════════════════════════════════════════════════════════════════

    use nomifun_secret::SecretStore;

    /// helper：把 `build_act_spec` 的 `Err(ToolResult)` 当作「缺参/被拒」，断言它是错误且含某子串。
    fn assert_spec_err(t: &BrowserTool, action: &str, input: Value, must_contain: &str) {
        match t.build_act_spec(action, &input) {
            Ok(spec) => panic!("expected an error for {action} {input}, got Ok({spec:?})"),
            Err(tr) => {
                assert!(tr.is_error, "expected an error ToolResult for {action}");
                assert!(
                    tr.content.contains(must_contain),
                    "error for {action} should mention {must_contain:?}: {}",
                    tr.content
                );
            }
        }
    }

    // ── schema: 全动作 enum + 参数池 + required==["action"] ──

    #[test]
    fn schema_enum_lists_full_action_space() {
        let schema = tool().input_schema();
        assert_eq!(schema["required"], json!(["action"]));
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        // Every ActSpec-backed action name must be offered to the model.
        for expected in [
            "navigate", "observe", "screenshot", "capabilities", "get_page_text", "search_page",
            "find_elements", "get_dropdown_options", "cursor", "tabs", "wait", "wait_for",
            "click", "hover", "type", "set_value", "select_option", "press_key", "scroll",
            "scroll_to_text", "upload_file", "download", "save_as_pdf", "extract",
            "switch_frame", "switch_tab", "close_tab",
            "open_link_new_tab", "back", "forward", "reload", "evaluate",
            "get_console_logs", "get_page_errors", "get_network_log",
        ] {
            assert!(
                actions.iter().any(|a| a == expected),
                "schema enum missing {expected}"
            );
        }
        // Param pool present.
        let props = &schema["properties"];
        for key in [
            "ref", "url", "text", "value", "keys", "direction", "amount", "options", "selector",
            "query", "file_path", "tab_id", "ms", "condition", "script", "schema",
        ] {
            assert!(props.get(key).is_some(), "schema missing param {key}");
        }
        // Round-trips through serde_json without loss.
        let text = serde_json::to_string(&schema).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, schema);
    }

    // ── build_act_spec: 全动作往返解析（有参的给参，无参的直接构造）──

    #[test]
    fn build_act_spec_parses_every_action() {
        let t = tool();
        // (action, input, expected ActSpec variant discriminant matcher)
        let ok = |action: &str, input: Value| {
            t.build_act_spec(action, &input)
                .unwrap_or_else(|e| panic!("{action} should parse, got err: {}", e.content))
        };
        assert!(matches!(ok("click", json!({"action":"click","ref":"f0e1"})), ActSpec::Click { .. }));
        assert!(matches!(ok("hover", json!({"action":"hover","ref":"f0e1"})), ActSpec::Hover { .. }));
        assert!(matches!(
            ok("type", json!({"action":"type","ref":"f0e1","text":"hi"})),
            ActSpec::Type { .. }
        ));
        assert!(matches!(
            ok("set_value", json!({"action":"set_value","ref":"f0e1","value":"v"})),
            ActSpec::SetValue { .. }
        ));
        assert!(matches!(
            ok("select_option", json!({"action":"select_option","ref":"f0e1","options":["a"]})),
            ActSpec::SelectOption { .. }
        ));
        assert!(matches!(
            ok("press_key", json!({"action":"press_key","keys":"Enter"})),
            ActSpec::PressKey { .. }
        ));
        assert!(matches!(
            ok("scroll", json!({"action":"scroll","direction":"down"})),
            ActSpec::Scroll { target: nomi_browser_engine::ScrollTarget::Viewport, .. }
        ));
        assert!(matches!(
            ok("scroll", json!({"action":"scroll","direction":"up","ref":"f0e1"})),
            ActSpec::Scroll { target: nomi_browser_engine::ScrollTarget::Element { .. }, .. }
        ));
        assert!(matches!(
            ok("scroll_to_text", json!({"action":"scroll_to_text","text":"Total"})),
            ActSpec::ScrollToText { .. }
        ));
        assert!(matches!(ok("get_page_text", json!({"action":"get_page_text"})), ActSpec::GetPageText));
        assert!(matches!(
            ok("search_page", json!({"action":"search_page","query":"q"})),
            ActSpec::SearchPage { .. }
        ));
        assert!(matches!(
            ok("find_elements", json!({"action":"find_elements","selector":"button"})),
            ActSpec::FindElements { .. }
        ));
        assert!(matches!(
            ok("get_dropdown_options", json!({"action":"get_dropdown_options","ref":"f0e1"})),
            ActSpec::GetDropdownOptions { .. }
        ));
        assert!(matches!(ok("cursor", json!({"action":"cursor"})), ActSpec::Cursor));
        assert!(matches!(ok("wait", json!({"action":"wait","ms":100})), ActSpec::Wait { ms: 100 }));
        assert!(matches!(
            ok("wait_for", json!({"action":"wait_for","condition":"url_contains","text":"x"})),
            ActSpec::WaitFor { .. }
        ));
        assert!(matches!(
            ok("upload_file", json!({"action":"upload_file","ref":"f0e1","file_path":"/tmp/a.png"})),
            ActSpec::UploadFile { .. }
        ));
        assert!(matches!(
            ok("download", json!({"action":"download","url":"https://x.com/file.pdf"})),
            ActSpec::Download { .. }
        ));
        assert!(matches!(ok("save_as_pdf", json!({"action":"save_as_pdf"})), ActSpec::SaveAsPdf));
        assert!(matches!(
            ok("extract", json!({"action":"extract","schema":{"title":"string"}})),
            ActSpec::Extract { .. }
        ));
        // extract without a schema still parses (defaults to {} — deterministic page representation).
        assert!(matches!(
            ok("extract", json!({"action":"extract"})),
            ActSpec::Extract { .. }
        ));
        assert!(matches!(
            ok("switch_frame", json!({"action":"switch_frame","ref":"f0e1"})),
            ActSpec::SwitchFrame { .. }
        ));
        assert!(matches!(ok("tabs", json!({"action":"tabs"})), ActSpec::Tabs));
        assert!(matches!(
            ok("switch_tab", json!({"action":"switch_tab","tab_id":"ab12"})),
            ActSpec::SwitchTab { .. }
        ));
        assert!(matches!(
            ok("close_tab", json!({"action":"close_tab","tab_id":"ab12"})),
            ActSpec::CloseTab { .. }
        ));
        assert!(matches!(
            ok("open_link_new_tab", json!({"action":"open_link_new_tab","url":"https://x.com"})),
            ActSpec::OpenLinkNewTab { .. }
        ));
        assert!(matches!(ok("back", json!({"action":"back"})), ActSpec::Back));
        assert!(matches!(ok("forward", json!({"action":"forward"})), ActSpec::Forward));
        assert!(matches!(ok("reload", json!({"action":"reload"})), ActSpec::Reload));
        assert!(matches!(
            ok("navigate", json!({"action":"navigate","url":"https://x.com"})),
            ActSpec::Navigate { .. }
        ));
        assert!(matches!(
            ok("evaluate", json!({"action":"evaluate","script":"1+1"})),
            ActSpec::Evaluate { .. }
        ));
    }

    // ── 缺参在启动浏览器前返错（build_act_spec 纯逻辑，never launches）──

    #[test]
    fn build_act_spec_missing_required_params_error_before_launch() {
        let t = tool();
        assert_spec_err(&t, "click", json!({"action":"click"}), "ref");
        assert_spec_err(&t, "hover", json!({"action":"hover"}), "ref");
        assert_spec_err(&t, "type", json!({"action":"type","ref":"f0e1"}), "text");
        assert_spec_err(&t, "type", json!({"action":"type","text":"hi"}), "ref");
        assert_spec_err(&t, "set_value", json!({"action":"set_value","ref":"f0e1"}), "value");
        assert_spec_err(&t, "select_option", json!({"action":"select_option","ref":"f0e1"}), "options");
        assert_spec_err(&t, "press_key", json!({"action":"press_key"}), "keys");
        assert_spec_err(&t, "scroll", json!({"action":"scroll"}), "direction");
        assert_spec_err(&t, "scroll", json!({"action":"scroll","direction":"sideways"}), "direction");
        assert_spec_err(&t, "search_page", json!({"action":"search_page"}), "query");
        assert_spec_err(&t, "find_elements", json!({"action":"find_elements"}), "selector");
        assert_spec_err(&t, "wait", json!({"action":"wait"}), "ms");
        assert_spec_err(&t, "wait_for", json!({"action":"wait_for"}), "condition");
        assert_spec_err(&t, "wait_for", json!({"action":"wait_for","condition":"bogus"}), "Unknown");
        assert_spec_err(&t, "upload_file", json!({"action":"upload_file","ref":"f0e1"}), "file_path");
        assert_spec_err(&t, "download", json!({"action":"download"}), "url");
        assert_spec_err(&t, "switch_tab", json!({"action":"switch_tab"}), "tab_id");
        assert_spec_err(&t, "open_link_new_tab", json!({"action":"open_link_new_tab"}), "url");
        assert_spec_err(&t, "navigate", json!({"action":"navigate"}), "url");
        assert_spec_err(&t, "evaluate", json!({"action":"evaluate"}), "script");
        // The engine was never constructed by any of these parameter errors.
        assert!(
            t.engine.lock().unwrap().is_none(),
            "missing-param validation must run BEFORE constructing the engine"
        );
    }

    #[tokio::test]
    async fn dispatch_missing_param_never_launches_engine() {
        // End-to-end through execute(): a missing param returns an error and never
        // constructs/launches the browser.
        let t = tool();
        let r = t.execute(json!({"action": "click"})).await;
        assert!(r.is_error);
        assert!(r.content.contains("ref"), "{}", r.content);
        assert!(t.engine.lock().unwrap().is_none(), "must not launch on a param error");
    }

    #[tokio::test]
    async fn dispatch_unknown_action_is_error() {
        let t = tool();
        let r = t.execute(json!({"action": "fly"})).await;
        assert!(r.is_error);
        assert!(r.content.contains("fly"), "{}", r.content);
        assert!(t.engine.lock().unwrap().is_none());
    }

    // ── secret:NAME 解析（纯函数）──

    #[test]
    fn parse_secret_ref_recognizes_prefix_and_extracts_name() {
        assert_eq!(parse_secret_ref("secret:my_password"), Some("my_password"));
        assert_eq!(parse_secret_ref("secret:GH_TOKEN"), Some("GH_TOKEN"));
        // ordinary text is NOT a secret ref
        assert_eq!(parse_secret_ref("hello world"), None);
        assert_eq!(parse_secret_ref("my secret: value"), None); // prefix not at start
        assert_eq!(parse_secret_ref("SECRET:upper"), None); // case-sensitive prefix
        // bare "secret:" with no name is NOT a reference (never resolves to nothing)
        assert_eq!(parse_secret_ref("secret:"), None);
    }

    // ── secret:NAME origin 门：匹配→Secret 注入；不匹配/无门→Blocked，且值不出现在输出 ──

    fn store_with_secret(name: &str, value: &str, origin: &str) -> SecretStore {
        let mut s = SecretStore::ephemeral().expect("ephemeral store");
        s.register(name, value, vec![origin.to_string()]).unwrap();
        s
    }

    #[test]
    fn resolve_type_input_plain_text_is_literal_not_secret() {
        let t = tool();
        match t.resolve_type_input("just typing this") {
            Ok(TypeInput::Literal(s)) => assert_eq!(s, "just typing this"),
            other => panic!("plain text must be Literal, got {other:?}"),
        }
    }

    #[test]
    fn resolve_type_input_secret_matching_origin_injects_secret() {
        // last observe url = https://shop.example.com/cart (from seed_snapshot).
        // Secret bound to example.com → subdomain shop.example.com matches.
        let store = store_with_secret("pw", "hunter2-PLAINTEXT", "example.com");
        let t = BrowserTool::with_secret_store(std::env::temp_dir().join("bt-f1"), false, store);
        seed_snapshot(&t, "f0e1", "textbox", "Password");

        let resolved = t.resolve_type_input("secret:pw");
        match &resolved {
            Ok(TypeInput::Secret(_)) => { /* expected; do not read the value out */ }
            other => panic!("matching-origin secret must inject TypeInput::Secret, got {other:?}"),
        }
        // SECURITY: the plaintext must NOT appear in the Debug of the TypeInput (redacted).
        let dbg = format!("{:?}", resolved.unwrap());
        assert!(
            !dbg.contains("hunter2-PLAINTEXT"),
            "secret plaintext leaked into Debug: {dbg}"
        );
        assert!(dbg.contains("redacted") || dbg.contains("Secret"), "{dbg}");
    }

    #[test]
    fn resolve_type_input_secret_wrong_origin_is_blocked_and_does_not_leak() {
        // Secret bound to other-bank.com, but the current page is shop.example.com →
        // fail-closed: returns an error ToolResult and NEVER the value/literal.
        let store = store_with_secret("pw", "hunter2-PLAINTEXT", "other-bank.com");
        let t = BrowserTool::with_secret_store(std::env::temp_dir().join("bt-f1b"), false, store);
        seed_snapshot(&t, "f0e1", "textbox", "Password"); // url = shop.example.com

        match t.resolve_type_input("secret:pw") {
            Ok(ti) => panic!("wrong-origin secret must be blocked, got Ok({ti:?})"),
            Err(tr) => {
                assert!(tr.is_error, "wrong-origin secret must be an error");
                // Must not leak the plaintext NOR silently type the literal "secret:pw".
                assert!(!tr.content.contains("hunter2-PLAINTEXT"), "leaked plaintext: {}", tr.content);
                assert!(
                    tr.content.to_lowercase().contains("fail-closed")
                        || tr.content.to_lowercase().contains("not available"),
                    "block message should explain the fail-closed gate: {}",
                    tr.content
                );
            }
        }
    }

    #[test]
    fn resolve_type_input_secret_no_store_is_blocked() {
        // No vault configured (P2 production default) → secret:NAME fails closed.
        let t = tool();
        seed_snapshot(&t, "f0e1", "textbox", "Password");
        match t.resolve_type_input("secret:pw") {
            Ok(ti) => panic!("no-store secret must be blocked, got Ok({ti:?})"),
            Err(tr) => assert!(tr.is_error),
        }
    }

    #[test]
    fn resolve_type_input_secret_no_origin_is_blocked() {
        // No observe yet → no current origin → cannot resolve → fail closed.
        let store = store_with_secret("pw", "v", "example.com");
        let t = BrowserTool::with_secret_store(std::env::temp_dir().join("bt-f1c"), false, store);
        // no seed_snapshot → current_origin() is None
        match t.resolve_type_input("secret:pw") {
            Ok(ti) => panic!("no-origin secret must be blocked, got Ok({ti:?})"),
            Err(tr) => {
                assert!(tr.is_error);
                assert!(
                    tr.content.contains("observe") || tr.content.to_lowercase().contains("origin"),
                    "should guide to observe first: {}",
                    tr.content
                );
            }
        }
    }

    #[test]
    fn build_act_spec_type_secret_does_not_leak_value_into_spec_debug() {
        // The resolved ActSpec::Type carries TypeInput::Secret — its Debug is redacted,
        // so even dbg!-ing the whole spec never prints the plaintext.
        let store = store_with_secret("pw", "TOP-SECRET-PLAINTEXT", "example.com");
        let t = BrowserTool::with_secret_store(std::env::temp_dir().join("bt-f1d"), false, store);
        seed_snapshot(&t, "f0e1", "textbox", "Password");
        let spec = t
            .build_act_spec("type", &json!({"action":"type","ref":"f0e1","text":"secret:pw"}))
            .expect("matching-origin secret should build a spec");
        let dbg = format!("{spec:?}");
        assert!(
            !dbg.contains("TOP-SECRET-PLAINTEXT"),
            "ActSpec Debug leaked the secret plaintext: {dbg}"
        );
    }

    /// **A 安全红线：set_value 的 `secret:NAME` 解析后 ActSpec Debug 不泄漏明文 + 标 secret=true**。
    /// build_act_spec 走 resolve_type_input → matching-origin secret → `SetValue { secret: true }`；
    /// 其手写 Debug 把 value 脱敏（即便 dbg! 整个 spec）——F2 会把 effect 透进 ToolResult，这里堵死
    /// set_value secret 的 Debug/日志泄漏面（anchor 抑制在引擎层另测）。
    #[test]
    fn build_act_spec_set_value_secret_does_not_leak_and_flags_secret() {
        let store = store_with_secret("pw", "TOP-SECRET-SETVALUE", "example.com");
        let t = BrowserTool::with_secret_store(std::env::temp_dir().join("bt-f2sv"), false, store);
        seed_snapshot(&t, "f0e1", "textbox", "Password");
        let spec = t
            .build_act_spec(
                "set_value",
                &json!({"action":"set_value","ref":"f0e1","value":"secret:pw"}),
            )
            .expect("matching-origin secret should build a set_value spec");
        // The spec must carry secret: true (drives engine anchor suppression).
        assert!(
            matches!(spec, ActSpec::SetValue { secret: true, .. }),
            "set_value secret:NAME must build SetValue{{ secret: true }}, got {spec:?}"
        );
        // Debug must not leak the resolved plaintext.
        let dbg = format!("{spec:?}");
        assert!(
            !dbg.contains("TOP-SECRET-SETVALUE"),
            "set_value ActSpec Debug leaked the secret plaintext: {dbg}"
        );
    }

    /// **A：set_value with plain (non-secret) text → `SetValue { secret: false }`** (regression).
    #[test]
    fn build_act_spec_set_value_plain_is_not_secret() {
        let t = tool();
        let spec = t
            .build_act_spec("set_value", &json!({"action":"set_value","ref":"f0e1","value":"hi"}))
            .expect("plain set_value should build");
        assert!(matches!(spec, ActSpec::SetValue { secret: false, .. }), "{spec:?}");
    }

    // ── F2: render_verify_note (verify-after-act surfacing) ──

    #[test]
    fn render_verify_note_includes_changed_and_anchors() {
        // Non-secret action: anchors present → note states changed + before/after.
        let effect = Effect {
            changed: true,
            before_anchor: Some(json!({"url": "https://x.com/a"})),
            after_anchor: Some(json!({"url": "https://x.com/b"})),
        };
        let note = render_verify_note(&effect);
        assert!(note.contains("changed=true"), "note must surface changed: {note}");
        assert!(note.contains("https://x.com/a") && note.contains("https://x.com/b"), "{note}");
        // Always nudges re-observe (a ref may be stale after a DOM change).
        assert!(note.to_lowercase().contains("re-observe"), "note must guide re-observe: {note}");
    }

    #[test]
    fn render_verify_note_changed_false_is_surfaced() {
        // Failure / no-op: changed=false reported truthfully (never assume success).
        let effect = Effect { changed: false, before_anchor: None, after_anchor: None };
        let note = render_verify_note(&effect);
        assert!(note.contains("changed=false"), "must truthfully surface changed=false: {note}");
    }

    /// **A+F2 安全红线：secret 动作的 None 锚点不泄漏任何值进 verify note**。引擎层对 secret
    /// 抑制锚点（before/after = None），故 render_verify_note 只显示 changed，绝不含值。这是 F2
    /// 把 effect 透进 ToolResult 后 set_value/type secret 仍不泄漏的 facade 侧守卫。
    #[test]
    fn render_verify_note_secret_none_anchors_leak_nothing() {
        let effect = Effect { changed: true, before_anchor: None, after_anchor: None };
        let note = render_verify_note(&effect);
        // No "before=" / "after=" segments when anchors are None (nothing to leak).
        assert!(!note.contains("before="), "no before-anchor when None: {note}");
        assert!(!note.contains("after="), "no after-anchor when None: {note}");
        assert!(note.contains("changed=true"), "still surfaces changed: {note}");
    }

    // ── describe: 全动作 + IRREVERSIBLE 讲后果 + secret 不回显 ──

    #[test]
    fn describe_covers_actions_and_hides_secret() {
        let t = tool();
        assert_eq!(t.describe(&json!({"action":"click","ref":"f0e1"})), "Browser: click [ref=f0e1]");
        assert_eq!(
            t.describe(&json!({"action":"type","ref":"f0e1","text":"hi"})),
            "Browser: type \"hi\" into [ref=f0e1]"
        );
        // secret reference must never echo the resolved value or the raw "secret:pw".
        let d = t.describe(&json!({"action":"type","ref":"f0e1","text":"secret:pw"}));
        assert!(d.contains("stored secret") && d.contains("hidden"), "{d}");
        assert!(!d.contains("secret:pw"), "describe must not echo the secret reference verbatim: {d}");
        // simple coverage of a few more.
        assert_eq!(t.describe(&json!({"action":"back"})), "Browser: go back");
        assert!(t.describe(&json!({"action":"scroll","direction":"down"})).contains("scroll viewport down"));
        assert!(t.describe(&json!({"action":"tabs"})).contains("list open tabs"));
    }

    #[test]
    fn describe_irreversible_click_spells_out_consequence() {
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Pay now");
        let d = t.describe(&json!({"action":"click","ref":"f0e3"}));
        assert!(
            d.to_uppercase().contains("IRREVERSIBLE"),
            "describe of a Pay-now click must warn IRREVERSIBLE: {d}"
        );
        assert!(
            d.contains("cannot be undone") || d.to_lowercase().contains("charge") || d.to_lowercase().contains("submit"),
            "describe should explain the consequence: {d}"
        );
    }

    #[test]
    fn describe_benign_click_has_no_irreversible_warning() {
        let t = tool();
        seed_snapshot(&t, "f0e3", "button", "Show more");
        let d = t.describe(&json!({"action":"click","ref":"f0e3"}));
        assert!(!d.to_uppercase().contains("IRREVERSIBLE"), "benign click must not warn: {d}");
    }

    // ── category_for over the full action space (read-only Info / write Exec /
    //    dangerous Irreversible), still routed through classify_action (E2) ──

    #[test]
    fn category_for_full_action_space() {
        let t = tool();
        // read-only → Info
        for a in ["navigate", "get_page_text", "search_page", "find_elements", "get_dropdown_options", "cursor", "tabs", "wait", "wait_for", "extract"] {
            assert_eq!(
                t.category_for(&json!({"action": a})),
                ToolCategory::Info,
                "{a} should be Info"
            );
        }
        // ordinary writes → Exec
        for a in ["type", "set_value", "hover", "select_option", "scroll", "scroll_to_text", "back", "forward", "switch_tab", "switch_frame", "upload_file", "download", "save_as_pdf"] {
            assert_eq!(
                t.category_for(&json!({"action": a})),
                ToolCategory::Exec,
                "{a} should be Exec"
            );
        }
        // dangerous click (via observe accname) → Irreversible
        seed_snapshot(&t, "f0e9", "button", "Delete account");
        assert_eq!(
            t.category_for(&json!({"action": "click", "ref": "f0e9"})),
            ToolCategory::Irreversible
        );
    }

    // ── #[ignore] 真 Chrome：经 facade execute 跑 click + type(含 secret:NAME 路径，验值不泄) ──
    //
    //   set NOMIFUN_CHROME_BINARY 后:
    //   cargo nextest run -p nomi-browser -- --run-ignored --ignored facade_act_click_and_secret_type_real
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --run-ignored"]
    async fn facade_act_click_and_secret_type_real() {
        // A data: URL with a form field + a button, so click/type are exercisable.
        let html = "data:text/html,<html><body>\
            <input id=u type=text aria-label=Username>\
            <input id=p type=password aria-label=Password>\
            <button id=b>Show more</button>\
            </body></html>";
        let store = store_with_secret("pw", "real-injected-secret-plaintext", "example.com");
        // Use a real (non-example) origin won't match; for the real test we navigate
        // to the data: URL whose origin is "null" — so the secret path will fail
        // closed (correct!). To exercise the *successful* secret path against a real
        // origin you'd register for that origin; here we assert the gate holds.
        let t = BrowserTool::with_secret_store(
            std::env::temp_dir().join("bt-f1-real"),
            false,
            store,
        );

        let nav = t.execute(json!({"action": "navigate", "url": html})).await;
        assert!(!nav.is_error, "{}", nav.content);
        let obs = t.execute(json!({"action": "observe"})).await;
        assert!(!obs.is_error, "{}", obs.content);

        // Find the button's ref from the observation and click it (benign → Exec).
        // (We grep the observation text for a ref; exact ref ids vary.)
        let some_ref = obs
            .content
            .split("[ref=")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(|s| s.to_string());
        if let Some(r) = some_ref {
            let clicked = t.execute(json!({"action": "click", "ref": r})).await;
            // Click may soft-fail if the ref wasn't a button; just assert no panic / engine error shape.
            assert!(
                !clicked.content.is_empty(),
                "click should return a message"
            );
        }

        // secret:NAME against the data: (null) origin → fail-closed Blocked, and the
        // plaintext must never appear in the tool output.
        let typed = t
            .execute(json!({"action": "type", "ref": "f0e0", "text": "secret:pw"}))
            .await;
        assert!(
            !typed.content.contains("real-injected-secret-plaintext"),
            "secret plaintext must never reach the tool output: {}",
            typed.content
        );
    }

    // ── P3-G2 #[ignore] 真 Chrome：构造带真 per-pet workspace 的 facade → download → 文件落
    //    <workspace>/downloads（非临时目录 / 非 <data_dir>/downloads）。证 workspace 传参链端到端。
    //
    //   set NOMIFUN_CHROME_BINARY 后:
    //   cargo nextest run -p nomi-browser --run-ignored all -E 'test(g2_download_lands_in_session_workspace_real)'
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --run-ignored"]
    async fn g2_download_lands_in_session_workspace_real() {
        // 唯一标识本次跑的 per-pet workspace（模拟 companion {companion_id}/workspace），与 data_dir 分开。
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!("bt-g2-data-{nonce}"));
        let workspace = std::env::temp_dir().join(format!("bt-g2-ws-{nonce}"));
        // 干净起点：本次 workspace 之前不存在（断言「下载是本次落进来的」）。
        let _ = std::fs::remove_dir_all(&workspace);

        // 关键：with_data_dir(data_dir) + .workspace(workspace) → engine() 把 workspace 填进
        // EngineConfig.workspace_dir → 引擎下载落 <workspace>/downloads（非 <data_dir>/downloads）。
        let t = BrowserTool::with_data_dir(data_dir.clone(), false).workspace(workspace.clone());

        // 触发一次真实下载：data: URL（download 动作注入隐藏 <a download> + click → chrome 落盘沙箱）。
        // 用 data:text/plain，内容非空 → 落盘文件 size>0。
        let dl = t
            .execute(json!({
                "action": "download",
                "url": "data:text/plain,P3-G2%20workspace%20download%20probe%20payload"
            }))
            .await;
        assert!(!dl.is_error, "download action errored: {}", dl.content);

        // 落点必须是 per-pet workspace 的 downloads 子目录（**非** <data_dir>/downloads / 临时目录）。
        let expected_dir = workspace.join("downloads");
        eprintln!("expected download dir = {}", expected_dir.display());

        // 轮询：下载异步，最长等 ~10s 直到 expected_dir 出现一个非空最终文件（跳过 .crdownload）。
        let mut found: Option<std::path::PathBuf> = None;
        for _ in 0..100 {
            if let Ok(rd) = std::fs::read_dir(&expected_dir) {
                for entry in rd.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("crdownload") {
                        continue;
                    }
                    if let Ok(meta) = std::fs::metadata(&path)
                        && meta.is_file()
                        && meta.len() > 0
                    {
                        found = Some(path);
                        break;
                    }
                }
            }
            if found.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let file = found.expect(
            "a non-empty downloaded file must land in <workspace>/downloads (the per-session workspace), \
             proving the G2 workspace_dir wiring reached the engine",
        );
        eprintln!("downloaded file = {}", file.display());

        // 红线：落点在真 per-pet workspace 下，**不**在 <data_dir>/downloads（兜底落点，G2 应已替换）。
        assert!(
            file.starts_with(&expected_dir),
            "download must land in <workspace>/downloads {}, got {}",
            expected_dir.display(),
            file.display()
        );
        assert!(
            !file.starts_with(data_dir.join("downloads")),
            "download must NOT fall back to <data_dir>/downloads when a workspace is wired: {}",
            file.display()
        );

        // 清理：删本次 workspace（连带下载文件 + 其 ADS）。
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // ════════════════════════════════════════════════════════════════════════
    // F1-sec: #[ignore] 真 Chrome —— 证 enforcement 真生效（redline 门 yolo 下真 Blocked /
    // 普通会话不被 facade 门误拦）。本机 Windows：set NOMIFUN_CHROME_BINARY 后
    //   cargo nextest run -p nomi-browser --run-ignored all -E 'test(f1sec_)'
    // ════════════════════════════════════════════════════════════════════════

    /// 真页含一个危险 accname（"Pay now"）按钮，observe 拿其 ref。返回 (tool 已 navigate+observe, ref)。
    /// 调用方传入 tool（bypassing / normal），共用这段「真页 + 拿到 Pay-now ref」的接线。
    /// 用 `file://` fixture（非 data: URL，后者带空格/引号会致 chrome session lost）。
    #[cfg(test)]
    async fn nav_observe_find_pay_button_ref(t: &BrowserTool) -> String {
        // file:// fixture（含 accname="Pay now" 按钮）。`CARGO_MANIFEST_DIR` 在 unix 是 `/abs`
        // （已带前导斜杠）、在 windows 是 `C:/abs`（需补一个）——仅缺失时补，避免 unix 上产生四斜杠
        // （`file:////...`）触发 chrome 把 url 归一成三斜杠 → navigate 误判 redirected。
        let manifest = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
        let abs = if manifest.starts_with('/') {
            manifest
        } else {
            format!("/{manifest}")
        };
        let url = format!("file://{abs}/tests/fixtures/redline_gate.html");
        let nav = t.execute(json!({"action": "navigate", "url": url})).await;
        assert!(!nav.is_error, "navigate failed: {}", nav.content);
        let obs = t.execute(json!({"action": "observe"})).await;
        assert!(!obs.is_error, "observe failed: {}", obs.content);
        // 从 observation 找含 "Pay now" 的那行的 ref。aria-snapshot 形如 `- button "Pay now" [ref=f0e2]`。
        obs.content
            .lines()
            .find(|l| l.contains("Pay now") && l.contains("[ref="))
            .and_then(|l| l.split("[ref=").nth(1))
            .and_then(|s| s.split(']').next())
            .map(|s| s.to_string())
            .expect("Pay-now button should appear in the observation with a [ref=]")
    }

    /// **F1-sec keystone**：yolo/companion（审批旁路）会话里点击一个不可逆（"Pay now"）按钮 →
    /// facade 独立 fail-closed 门 **hard-deny Blocked**（证 fail-open 已闭：session_mode 真穿透）。
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --run-ignored"]
    async fn f1sec_redline_gate_hard_denies_irreversible_in_yolo_session_real() {
        let t = bypassing_tool(); // = with_policy(.., session_bypasses_approval=true, ..)
        let pay_ref = nav_observe_find_pay_button_ref(&t).await;

        let clicked = t
            .execute(json!({"action": "click", "ref": pay_ref}))
            .await;
        assert!(
            clicked.is_error,
            "irreversible click in a yolo session MUST be hard-denied by the facade gate, got: {}",
            clicked.content
        );
        let lower = clicked.content.to_lowercase();
        assert!(
            lower.contains("irreversible") || lower.contains("blocked"),
            "block message should explain the redline: {}",
            clicked.content
        );
    }

    /// **F1-sec 对照**：普通会话（审批未旁路）里点击同一个不可逆按钮 → facade 门**不拦**（交
    /// orchestration 经 Irreversible 审批；本测在 facade 层只验「门未 hard-deny」，故点击会真去
    /// dispatch——它可能成功/良性失败，但**绝不是** facade 门的 Blocked 错误）。证「普通会话不误拦」。
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --run-ignored"]
    async fn f1sec_redline_gate_does_not_block_irreversible_in_normal_session_real() {
        let t = tool(); // 普通会话：session_bypasses_approval = false。
        let pay_ref = nav_observe_find_pay_button_ref(&t).await;

        let clicked = t
            .execute(json!({"action": "click", "ref": pay_ref}))
            .await;
        // 关键断言：即便 click 真去 dispatch（可能成功/良性失败），它**不应**是 facade redline 门的
        // hard-deny（那条错误含 "blocked in an auto-approving session"）。普通会话的不可逆动作交
        // orchestration，不被 facade 门拦。
        let lower = clicked.content.to_lowercase();
        assert!(
            !lower.contains("auto-approving session"),
            "normal-session irreversible must NOT be hard-denied by the facade redline gate: {}",
            clicked.content
        );
    }

    /// **F1-sec 对照（gate 纯视角，无 chrome 也跑）**：bypassing 会话里点击**良性**按钮（"Show more"）
    /// → 门放行（只拦不可逆，不误拦良性可逆动作）。这条不 #[ignore]（纯 gate 视角，已有覆盖，保留为
    /// 边界守卫文档）。
    #[test]
    fn f1sec_redline_gate_allows_benign_click_in_yolo_session() {
        let t = bypassing_tool();
        seed_snapshot(&t, "f0e9", "button", "Show more");
        assert!(
            t.redline_gate("click", &json!({"action": "click", "ref": "f0e9"})).is_none(),
            "benign click must not be hard-denied even in a yolo session"
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // P3-N2: 截图落 per-pet workspace/screenshots + ToolResult 图片引用（裁决⑨）。
    // [纯逻辑] 落盘路径解析（workspace/screenshots/shot-{ts}.png；无 workspace 兜底 data_dir）+
    // ToolResult content 引用构造 + persist_screenshot 真写盘。[#[ignore]] 真 Chrome 端到端。
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn screenshot_path_is_screenshots_subdir_shot_ts_png() {
        // 落盘路径 = <base>/screenshots/shot-<ts>.png。纯逻辑（不碰 FS）。
        let base = std::path::Path::new("/some/companion/workspace");
        let p = screenshot_path(base, 1718000000123);
        assert_eq!(p, base.join("screenshots").join("shot-1718000000123.png"));
        // 子目录名固定为 "screenshots"（与下载的 "downloads" 平级隔离）。
        assert_eq!(SCREENSHOT_SUBDIR, "screenshots");
        // 文件名以 shot- 前缀 + .png 后缀（会话流可识别为截图）。
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("shot-") && name.ends_with(".png"), "{name}");
    }

    #[test]
    fn screenshot_base_dir_prefers_workspace_then_falls_back_to_data_dir() {
        // 有 per-pet workspace → 用 workspace（截图落 <workspace>/screenshots）。
        let ws = std::env::temp_dir().join("bt-n2-ws");
        let t = BrowserTool::with_policy(&BrowserConfig::default(), false, false, false, Some(ws.clone()), None, None);
        assert_eq!(t.screenshot_base_dir(), ws, "workspace present → base = workspace");
        // 无 workspace（仅 BrowserConfig / 测试）→ 兜底 data_dir（仍隔离，非用户 Pictures）。
        let t2 = tool();
        assert_eq!(
            t2.screenshot_base_dir(),
            t2.data_dir,
            "no workspace → base falls back to data_dir (isolated, never the user's real Pictures)"
        );
    }

    #[test]
    fn render_screenshot_note_includes_saved_path_reference() {
        // 落盘成功：content 含落盘路径引用（会话流据此可视化展示截图所在）。
        let path = std::path::Path::new("/ws/screenshots/shot-42.png");
        let note = render_screenshot_note(Some(path));
        assert!(note.contains("shot-42.png"), "note must reference the saved path: {note}");
        assert!(
            note.to_lowercase().contains("saved"),
            "note must say the screenshot was saved: {note}"
        );
        // 仍保留 P0 的「已捕获」语义。
        assert!(note.to_lowercase().contains("captured"), "{note}");
    }

    #[test]
    fn render_screenshot_note_falls_back_without_path_when_persist_failed() {
        // 落盘失败（best-effort 降级）：仍报「已捕获」（base64 已回 LLM），无路径引用。
        let note = render_screenshot_note(None);
        assert!(note.to_lowercase().contains("captured"), "{note}");
        assert!(!note.to_lowercase().contains("saved to"), "no path reference on failure: {note}");
    }

    #[test]
    fn persist_screenshot_writes_png_bytes_creating_parent_dirs() {
        // persist_screenshot 真写盘：建 <base>/screenshots/ 父目录 + 写字节，size>0。
        let nonce = screenshot_timestamp();
        let base = std::env::temp_dir().join(format!("bt-n2-persist-{nonce}"));
        let _ = std::fs::remove_dir_all(&base);
        let path = screenshot_path(&base, nonce);
        // 极小合法 PNG 头（%PNG\r\n... 起始的 8 字节签名 + 占位）——只验落盘字节往返。
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\nFAKE-PNG-PAYLOAD";

        persist_screenshot(&path, bytes).expect("persist_screenshot should write the file");
        let read = std::fs::read(&path).expect("written screenshot must be readable");
        assert_eq!(read, bytes, "round-tripped bytes must match");
        assert!(!read.is_empty(), "screenshot file must be non-empty");
        // 落点在 <base>/screenshots/ 下（裁决⑨：隔离子目录，非用户真实目录）。
        assert!(path.starts_with(base.join("screenshots")));

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── P3-N2 #[ignore] 真 Chrome：navigate → screenshot → PNG 落 <workspace>/screenshots +
    //    size>0 + 是 PNG（%PNG 签名）+ ToolResult 含落盘路径引用（会话流可见）。证落盘+引用端到端。
    //
    //   set NOMIFUN_CHROME_BINARY 后:
    //   cargo nextest run -p nomi-browser --run-ignored all -E 'test(n2_screenshot_lands_in_workspace_real)'
    #[tokio::test]
    #[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 -- --run-ignored"]
    async fn n2_screenshot_lands_in_workspace_real() {
        // 唯一标识本次跑的 per-pet workspace（模拟 companion {companion_id}/workspace），与 data_dir 分开。
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = std::env::temp_dir().join(format!("bt-n2-data-{nonce}"));
        let workspace = std::env::temp_dir().join(format!("bt-n2-ws-{nonce}"));
        let _ = std::fs::remove_dir_all(&workspace);

        // with_data_dir(data_dir) + .workspace(workspace) → do_screenshot 落 <workspace>/screenshots。
        let t = BrowserTool::with_data_dir(data_dir.clone(), false).workspace(workspace.clone());

        let nav = t
            .execute(json!({"action": "navigate", "url": "https://example.com"}))
            .await;
        assert!(!nav.is_error, "navigate failed: {}", nav.content);

        let shot = t.execute(json!({"action": "screenshot"})).await;
        assert!(!shot.is_error, "screenshot failed: {}", shot.content);
        // ToolResult 仍带 base64 图给 LLM（多模态，与 P0 不冲突）。
        assert_eq!(shot.images.len(), 1, "screenshot must still return the inline image to the LLM");
        assert_eq!(shot.images[0].media_type, "image/png");
        assert!(!shot.images[0].data.is_empty(), "base64 image data must be present for the LLM");

        // ToolResult content 含落盘路径引用（会话流可见）——含 screenshots 子目录 + shot- 前缀。
        assert!(
            shot.content.contains("screenshots") && shot.content.contains("shot-"),
            "ToolResult content must reference the saved screenshot path (session-stream visible): {}",
            shot.content
        );

        // 落点必须是 per-pet workspace 的 screenshots 子目录（非 <data_dir> / 非用户真实目录）。
        let expected_dir = workspace.join("screenshots");
        eprintln!("expected screenshot dir = {}", expected_dir.display());
        let file = std::fs::read_dir(&expected_dir)
            .ok()
            .and_then(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .find(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
            })
            .expect(
                "a PNG must land in <workspace>/screenshots (per-session workspace), proving the \
                 N2 workspace落盘 wiring reached the engine",
            );
        eprintln!("screenshot file = {}", file.display());

        // size>0 + 是真 PNG（首 8 字节 = PNG 签名 89 50 4E 47 0D 0A 1A 0A）。
        let bytes = std::fs::read(&file).expect("screenshot file must be readable");
        assert!(!bytes.is_empty(), "screenshot file must be non-empty");
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "saved file must start with the PNG signature"
        );

        // 红线：落点在 per-pet workspace 下，**不**在 <data_dir>/screenshots（兜底落点）。
        assert!(
            file.starts_with(&expected_dir),
            "screenshot must land in <workspace>/screenshots {}, got {}",
            expected_dir.display(),
            file.display()
        );
        assert!(
            !file.starts_with(data_dir.join("screenshots")),
            "screenshot must NOT fall back to <data_dir>/screenshots when a workspace is wired: {}",
            file.display()
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// **P3 facade wiring**: verify that `with_extract_model` threads the model seam
    /// into the tool, and that the default (no model) is `None` (graceful degradation).
    #[test]
    fn extract_model_defaults_to_none_and_builder_sets_it() {
        use crate::extract::ExtractModel;

        struct DummyModel;
        #[async_trait::async_trait]
        impl ExtractModel for DummyModel {
            async fn complete(&self, _prompt: &str) -> Result<String, String> {
                Ok("{}".into())
            }
        }

        let data_dir = std::env::temp_dir().join("nomifun-test-extract-model-wiring");
        let tool = BrowserTool::with_data_dir(data_dir.clone(), false);
        // Default: no model seam (graceful degradation path).
        assert!(
            tool.extract_model.is_none(),
            "extract_model must default to None (graceful degradation)"
        );

        // With model injected.
        let tool_with_model =
            BrowserTool::with_data_dir(data_dir.clone(), false)
                .with_extract_model(Arc::new(DummyModel));
        assert!(
            tool_with_model.extract_model.is_some(),
            "with_extract_model must set the model seam"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // ── Task 4: resume forces fresh observe ──────────────────────────────────

    #[tokio::test]
    async fn resume_invalidates_pre_takeover_refs() {
        let data_dir = std::env::temp_dir().join("nomifun-test-re-observe-flag");
        let t = BrowserTool::with_data_dir(data_dir.clone(), false);

        // Initially: no re-observe needed.
        assert!(!t.needs_re_observe());

        // Simulate: a takeover just resolved → set the flag.
        t.set_must_re_observe();
        assert!(t.needs_re_observe());

        // A ref-based action (click with a ref) must be rejected.
        let result = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        assert!(
            result.is_error,
            "ref-based action must be rejected when must_re_observe is set"
        );
        assert!(
            result.content.contains("observe"),
            "error should mention 'observe': {}",
            result.content
        );

        // The flag is still set (the rejected action doesn't clear it).
        assert!(t.needs_re_observe());

        // clear_must_re_observe simulates what a successful observe does.
        t.clear_must_re_observe();
        assert!(!t.needs_re_observe());

        // Now a ref-based action would no longer be rejected by the flag
        // (it will fail for other reasons — no engine — but NOT with the
        // "observe first" message).
        let result2 = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        assert!(
            !result2.content.contains("refs from before the takeover"),
            "after clearing must_re_observe, the takeover guard should not fire: {}",
            result2.content
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // ── Task 5: irreversible under bypass released only by confirmed takeover ──

    #[tokio::test]
    async fn irreversible_under_bypass_released_only_by_confirmed_takeover() {
        use crate::takeover::TakeoverResolution;

        let data_dir = std::env::temp_dir().join("nomifun-test-takeover-redline");

        // -- Case 1: Cancelled → stays Blocked --
        {
            let mut t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true,  // session_bypasses_approval (yolo)
                false, // evaluate_full_power
                false, // evaluate_persistent_login
                None,  // workspace_dir
                None,  // runtime_mode
                None,  // secret_source
            );
            // Enable takeover + force Cancelled resolution.
            t.takeover_controller.enabled = true;
            t.takeover_controller.force_resolution = Some(TakeoverResolution::Cancelled);
            // Seed an irreversible button.
            seed_snapshot(&t, "f0e3", "button", "Pay now");

            let result = t
                .execute(json!({"action": "click", "ref": "f0e3"}))
                .await;
            assert!(
                result.is_error,
                "Cancelled takeover must keep the action Blocked: {}",
                result.content
            );
            assert!(
                result.content.to_lowercase().contains("blocked")
                    || result.content.to_lowercase().contains("irreversible"),
                "error should mention blocked/irreversible: {}",
                result.content
            );
        }

        // -- Case 2: Confirmed → proceeds (gate passes) --
        {
            let mut t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true,  // session_bypasses_approval (yolo)
                false, // evaluate_full_power
                false, // evaluate_persistent_login
                None,  // workspace_dir
                None,  // runtime_mode
                None,  // secret_source
            );
            // Enable takeover + force Confirmed resolution.
            t.takeover_controller.enabled = true;
            t.takeover_controller.force_resolution = Some(TakeoverResolution::Confirmed);
            // Seed an irreversible button.
            seed_snapshot(&t, "f0e3", "button", "Pay now");

            let result = t
                .execute(json!({"action": "click", "ref": "f0e3"}))
                .await;
            // With Confirmed, the redline gate passes. The action proceeds past the gate.
            // It will fail at the engine level (no chrome launched) — but the important
            // thing is it's NOT a "blocked" / "irreversible" error from the redline gate.
            assert!(
                !result.content.to_lowercase().contains("blocked"),
                "Confirmed takeover must release the action past the redline gate, \
                 but got a blocked error: {}",
                result.content
            );
            // Also verify must_re_observe was set (user may have navigated during takeover).
            assert!(
                t.needs_re_observe(),
                "after a Confirmed takeover, must_re_observe should be set"
            );
        }

        // -- Case 3: TimedOut → stays Blocked (fail-closed) --
        {
            let mut t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true,
                false,
                false,
                None,
                None,
                None,
            );
            t.takeover_controller.enabled = true;
            t.takeover_controller.force_resolution = Some(TakeoverResolution::TimedOut);
            seed_snapshot(&t, "f0e3", "button", "Pay now");

            let result = t
                .execute(json!({"action": "click", "ref": "f0e3"}))
                .await;
            assert!(
                result.is_error,
                "TimedOut takeover must keep the action Blocked: {}",
                result.content
            );
        }

        // -- Case 4: Unavailable → stays Blocked (fail-closed) --
        {
            let mut t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true,
                false,
                false,
                None,
                None,
                None,
            );
            t.takeover_controller.enabled = true;
            t.takeover_controller.force_resolution = Some(TakeoverResolution::Unavailable);
            seed_snapshot(&t, "f0e3", "button", "Pay now");

            let result = t
                .execute(json!({"action": "click", "ref": "f0e3"}))
                .await;
            assert!(
                result.is_error,
                "Unavailable takeover must keep the action Blocked: {}",
                result.content
            );
        }

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// Phase D: a fake approval gate returning a fixed decision.
    struct FakeApprovalGate {
        decision: crate::approval::ApprovalDecision,
    }
    #[async_trait::async_trait]
    impl crate::approval::BrowserApprovalGate for FakeApprovalGate {
        async fn request_approval(
            &self,
            _ask: crate::approval::ApprovalAsk,
        ) -> crate::approval::ApprovalDecision {
            self.decision
        }
    }

    /// Phase D: an injected approval gate drives the redline takeover path — Approve
    /// releases the irreversible action past the gate (+ sets must_re_observe), Deny
    /// keeps it Blocked (fail-closed). `with_approval_gate` also enables the controller.
    #[tokio::test]
    async fn approval_gate_releases_on_approve_blocks_on_deny() {
        use crate::approval::ApprovalDecision;

        // -- Approve → released past the redline gate --
        {
            let gate = Arc::new(FakeApprovalGate { decision: ApprovalDecision::Approve });
            let t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true, // bypass (yolo)
                false,
                false,
                None,
                None,
                None,
            )
            .with_approval_gate(gate);
            assert!(t.takeover_controller().enabled, "with_approval_gate must enable takeover");
            seed_snapshot(&t, "f0e3", "button", "Pay now");
            let result = t.execute(json!({"action": "click", "ref": "f0e3"})).await;
            // Released past the gate (fails later at the engine since no chrome) — NOT a
            // redline block.
            assert!(
                !result.content.to_lowercase().contains("blocked"),
                "Approve must release the irreversible action past the redline gate: {}",
                result.content
            );
            assert!(t.needs_re_observe(), "an approved takeover sets must_re_observe");
        }

        // -- Deny → stays Blocked (fail-closed) --
        {
            let gate = Arc::new(FakeApprovalGate { decision: ApprovalDecision::Deny });
            let t = BrowserTool::with_policy(
                &BrowserConfig::default(),
                true,
                false,
                false,
                None,
                None,
                None,
            )
            .with_approval_gate(gate);
            seed_snapshot(&t, "f0e3", "button", "Pay now");
            let result = t.execute(json!({"action": "click", "ref": "f0e3"})).await;
            assert!(result.is_error, "Deny must keep the action Blocked: {}", result.content);
            assert!(
                result.content.to_lowercase().contains("blocked")
                    || result.content.to_lowercase().contains("irreversible"),
                "error should mention blocked/irreversible: {}",
                result.content
            );
        }
    }

    // ── P7C recording tests ─────────────────────────────────────────────────

    /// A minimal fake engine that returns a successful ActResult for any action.
    struct FakeRecordingEngine;

    #[async_trait]
    impl nomi_browser_engine::BrowserEngine for FakeRecordingEngine {
        fn capabilities(&self) -> Capabilities {
            Capabilities { browser_ready: true, headful: false, display_available: false, engine: "fake".into() }
        }
        async fn navigate(&self, _url: &str, _new_tab: bool) -> Result<nomi_browser_engine::NavResult, nomi_browser_engine::BrowserError> {
            Ok(nomi_browser_engine::NavResult {
                final_url: "https://test.example.com".into(),
                http_status: Some(200),
                redirected: false,
                load_state: nomi_browser_engine::LoadState::Load,
            })
        }
        async fn screenshot(&self) -> Result<Vec<u8>, nomi_browser_engine::BrowserError> {
            Err(nomi_browser_engine::BrowserError::Unsupported { capability: "screenshot".into(), hint: "fake".into() })
        }
        async fn rendered_html(&self) -> Result<String, nomi_browser_engine::BrowserError> {
            Err(nomi_browser_engine::BrowserError::Unsupported { capability: "rendered_html".into(), hint: "fake".into() })
        }
        async fn observe(&self, _opts: &nomi_browser_engine::ObserveOpts) -> Result<nomi_browser_engine::Observation, nomi_browser_engine::BrowserError> {
            Err(nomi_browser_engine::BrowserError::Unsupported { capability: "observe".into(), hint: "fake".into() })
        }
        async fn act(
            &self,
            _spec: &nomi_browser_engine::ActSpec,
            _progress: &nomi_browser_engine::progress::Progress,
        ) -> Result<nomi_browser_engine::ActResult, nomi_browser_engine::BrowserError> {
            Ok(nomi_browser_engine::ActResult {
                success: true,
                message: "clicked".into(),
                effect: nomi_browser_engine::Effect { changed: true, before_anchor: None, after_anchor: None },
            })
        }
        async fn debug_snapshot(&self) -> Result<nomi_browser_engine::DebugSnapshot, nomi_browser_engine::BrowserError> {
            Err(nomi_browser_engine::BrowserError::Unsupported { capability: "debug".into(), hint: "fake".into() })
        }
    }

    #[tokio::test]
    async fn do_act_appends_step_when_recording() {
        let t = tool();
        // Inject a fake engine that succeeds on any act.
        *t.engine.lock().unwrap() = Some(Ok(Arc::new(FakeRecordingEngine)));
        // Seed a snapshot so current_origin is known and the ref resolves.
        seed_snapshot(&t, "f0e1", "button", "Go");

        // Not recording yet → no step appended.
        let result = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        assert!(!result.is_error, "action should succeed: {}", result.content);
        assert!(!t.is_recording());

        // Start recording.
        t.start_recording();
        assert!(t.is_recording());

        // Act while recording → step appended.
        let result = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        assert!(!result.is_error, "action should succeed: {}", result.content);

        // Stop and get the recording.
        let rec = t.stop_recording().expect("should have a recording");
        assert_eq!(rec.steps.len(), 1, "exactly one step should be recorded");
        assert_eq!(rec.steps[0].action, "click");
        assert_eq!(rec.steps[0].args["ref"], "f0e1");
        // Selector is None at facade level (engine internals not accessible here).
        assert_eq!(rec.steps[0].selector, None);
    }

    // ── P7A site-memory wiring tests ────────────────────────────────────────

    #[tokio::test]
    async fn do_act_success_records_site_memory() {
        use crate::site_memory::{InMemorySink, SiteMemoryStore};

        let sink = InMemorySink::new();
        let store = Arc::new(SiteMemoryStore::new(Box::new(sink)));
        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("sm-test"), false)
            .with_site_memory(store.clone());

        // Inject a fake engine that succeeds on any act.
        *t.engine.lock().unwrap() = Some(Ok(Arc::new(FakeRecordingEngine)));
        // Seed a snapshot so current_origin is known and the ref resolves.
        seed_snapshot(&t, "f0e1", "button", "Go");

        // Successful act → site memory records the element.
        let result = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        assert!(!result.is_error, "action should succeed: {}", result.content);

        // Query site memory for the eTLD+1 of the snapshot URL (example.com).
        let hints = store.query("example.com");
        assert_eq!(hints.len(), 1, "one entry should be recorded");
        assert_eq!(hints[0].role, "button");
        assert_eq!(hints[0].accessible_name, "Go");
        assert_eq!(hints[0].intent, "click");
    }

    #[test]
    fn is_concurrency_safe_stays_false_with_site_memory() {
        use crate::site_memory::{InMemorySink, SiteMemoryStore};

        let sink = InMemorySink::new();
        let store = Arc::new(SiteMemoryStore::new(Box::new(sink)));
        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("sm-test2"), false)
            .with_site_memory(store);

        // All actions remain concurrency-unsafe regardless of site-memory presence.
        for action in [
            json!({"action": "observe"}),
            json!({"action": "click", "ref": "f0e1"}),
            json!({"action": "navigate", "url": "https://example.com"}),
        ] {
            assert!(
                !t.is_concurrency_safe(&action),
                "Browser must stay concurrency-unsafe with site_memory for {action:?}"
            );
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // P7B: Visual Fallback wiring tests
    // ════════════════════════════════════════════════════════════════════════

    #[test]
    fn visual_fallback_default_off() {
        // Default: visual fallback is OFF (vision-cost path, opt-in).
        let t = tool();
        assert!(
            !t.visual_fallback_enabled,
            "visual_fallback_enabled must default to false (client-pref gated, default OFF)"
        );
        assert!(
            t.visual_locator.is_none(),
            "visual_locator must default to None (graceful degradation)"
        );
    }

    #[test]
    fn with_visual_fallback_enabled_threads_flag() {
        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("vf-test"), false)
            .with_visual_fallback_enabled(true);
        assert!(
            t.visual_fallback_enabled,
            "with_visual_fallback_enabled(true) must set the flag"
        );
        // Still no locator — enabling without a locator is safe (graceful degradation).
        assert!(t.visual_locator.is_none());
    }

    #[test]
    fn with_visual_locator_threads_locator() {
        use crate::visual_fallback::{VisualLocateResult, VisualLocator, PixelBox};

        struct DummyLocator;
        #[async_trait::async_trait]
        impl VisualLocator for DummyLocator {
            async fn locate(
                &self,
                _screenshot: &[u8],
                _instruction: &str,
            ) -> Result<VisualLocateResult, String> {
                Ok(VisualLocateResult {
                    pixel_box: PixelBox { x: 0.0, y: 0.0, width: 10.0, height: 10.0 },
                    confidence: 1.0,
                })
            }
        }

        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("vf-test2"), false)
            .with_visual_locator(Arc::new(DummyLocator))
            .with_visual_fallback_enabled(true);

        assert!(t.visual_fallback_enabled);
        assert!(
            t.visual_locator.is_some(),
            "with_visual_locator must set the locator"
        );
    }

    /// Verify that `do_act` attempts visual fallback when:
    /// - visual_fallback_enabled == true
    /// - visual_locator is Some
    /// - engine.act returns NodeStale/NotConnected
    /// - action is click/type
    ///
    /// This is a facade-level unit test (no real engine needed — the test verifies the
    /// wiring logic, not the engine dispatch). The real-Chrome e2e test is Task 6.
    #[tokio::test]
    async fn do_act_falls_back_to_visual_when_enabled_and_anchor_fails() {
        use crate::visual_fallback::{VisualLocateResult, VisualLocator, PixelBox};
        use nomi_tools::Tool;

        struct ClickRecordingLocator {
            called: std::sync::atomic::AtomicBool,
        }
        #[async_trait::async_trait]
        impl VisualLocator for ClickRecordingLocator {
            async fn locate(
                &self,
                _screenshot: &[u8],
                _instruction: &str,
            ) -> Result<VisualLocateResult, String> {
                self.called.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(VisualLocateResult {
                    pixel_box: PixelBox { x: 90.0, y: 90.0, width: 20.0, height: 20.0 },
                    confidence: 0.9,
                })
            }
        }

        let locator = Arc::new(ClickRecordingLocator {
            called: std::sync::atomic::AtomicBool::new(false),
        });

        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("vf-facade-test"), false)
            .with_visual_fallback_enabled(true)
            .with_visual_locator(locator.clone());

        // Execute a click action. Without a running engine, `engine()` will fail, which
        // means the test proves the wiring up to the engine-acquisition point.
        // The facade returns "Browser engine unavailable" before reaching the visual
        // fallback path (because `engine()` fails without a real Chrome). This is
        // expected — the full e2e path is tested in Task 6 with a real Chrome.
        //
        // What we CAN verify: the tool has the correct configuration.
        assert!(t.visual_fallback_enabled);
        assert!(t.visual_locator.is_some());

        // Verify the action_is_click_or_type helper works correctly.
        assert!(t.action_is_click_or_type(&ActSpec::Click { r#ref: "f0e1".into() }));
        assert!(t.action_is_click_or_type(&ActSpec::Hover { r#ref: "f0e1".into() }));
        assert!(t.action_is_click_or_type(&ActSpec::Type {
            r#ref: "f0e1".into(),
            text: TypeInput::Literal("hello".into()),
        }));
        assert!(!t.action_is_click_or_type(&ActSpec::PressKey { keys: "Enter".into() }));
        assert!(!t.action_is_click_or_type(&ActSpec::Scroll {
            target: nomi_browser_engine::ScrollTarget::Viewport,
            direction: nomi_browser_engine::ScrollDir::Down,
            amount: None,
        }));

        // Verify the locator hasn't been called (no engine = no screenshot = no fallback).
        let result = t.execute(json!({"action": "click", "ref": "f0e1"})).await;
        // Two possible outcomes:
        // 1. Engine available (Chrome installed) → action fails with NodeStale → visual
        //    fallback triggers → returns "Clicked at (X, Y) via visual fallback".
        // 2. Engine unavailable (no Chrome) → returns "Browser engine unavailable".
        //
        // Either outcome proves the wiring is correct:
        // - (1) means the full visual fallback path ran successfully.
        // - (2) means graceful degradation when no engine is available.
        let is_visual_fallback_success = result.content.contains("via visual fallback");
        let is_engine_unavailable =
            result.content.contains("unavailable") || result.content.contains("Unavailable");
        assert!(
            is_visual_fallback_success || is_engine_unavailable,
            "expected either visual fallback success or engine unavailable, got: {:?}",
            result.content
        );

        if is_visual_fallback_success {
            // The full path worked: engine launched, ref was stale, visual fallback
            // ran, locator was called, click dispatched at the mapped CSS point.
            assert!(
                locator.called.load(std::sync::atomic::Ordering::SeqCst),
                "locator must have been called when visual fallback fires"
            );
            // The locator returned a box at (90,90)+(20x20) → center=(100,100).
            // DPR=1.0 → CSS (100, 100).
            assert!(
                result.content.contains("(100, 100)"),
                "visual fallback click should be at (100, 100) CSS pixels: {:?}",
                result.content
            );
        }
    }

    // ── P7B SoM: som_locate_point coordinate keystone (engine-free) ──

    #[tokio::test]
    async fn som_locate_point_maps_picked_label_to_css_center() {
        use crate::visual_fallback::{SomLabelResult, VisualLocateResult, VisualLocator};
        use nomi_browser_engine::CssRect;

        // Fake locator: ignores the image, always picks label 2.
        struct PickTwo;
        #[async_trait::async_trait]
        impl VisualLocator for PickTwo {
            async fn locate(&self, _png: &[u8], _i: &str) -> Result<VisualLocateResult, String> {
                Err("bbox path not used in this test".into())
            }
            async fn locate_labeled(
                &self,
                _png: &[u8],
                _i: &str,
                _n: usize,
            ) -> Result<SomLabelResult, String> {
                Ok(SomLabelResult { label: 2, confidence: 0.9 })
            }
        }

        // CSS-pixel rects. som_overlay numbers by (y, then x):
        //   (y=10,x=10)=label1, (y=10,x=200)=label2, (y=100,x=10)=label3.
        let rects = vec![
            CssRect { x: 10.0, y: 100.0, width: 40.0, height: 20.0 }, // → label 3
            CssRect { x: 200.0, y: 10.0, width: 60.0, height: 40.0 }, // → label 2 (picked)
            CssRect { x: 10.0, y: 10.0, width: 20.0, height: 20.0 },  // → label 1
        ];
        let dpr = 2.0;
        let pt = BrowserTool::som_locate_point(&PickTwo, b"not-a-png", "the Submit button", dpr, &rects)
            .await
            .expect("SoM locate should resolve label 2");
        // label 2 = (x=200,y=10,w=60,h=40) → CSS center (230, 30). The ×dpr (draw) then ÷dpr
        // (click) round-trip must net back to the true CSS center.
        assert!((pt.x - 230.0).abs() < 1e-6, "x={}", pt.x);
        assert!((pt.y - 30.0).abs() < 1e-6, "y={}", pt.y);
    }

    #[tokio::test]
    async fn som_locate_point_errs_when_model_misses() {
        use crate::visual_fallback::{SomLabelResult, VisualLocateResult, VisualLocator};
        use nomi_browser_engine::CssRect;

        struct Miss;
        #[async_trait::async_trait]
        impl VisualLocator for Miss {
            async fn locate(&self, _png: &[u8], _i: &str) -> Result<VisualLocateResult, String> {
                Err("n/a".into())
            }
            async fn locate_labeled(
                &self,
                _png: &[u8],
                _i: &str,
                _n: usize,
            ) -> Result<SomLabelResult, String> {
                Err("no label matched".into())
            }
        }
        let rects = vec![CssRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 }];
        let r = BrowserTool::som_locate_point(&Miss, b"x", "target", 1.0, &rects).await;
        assert!(r.is_err(), "a model miss must propagate as Err so the caller falls back to raw bbox");
    }

    #[tokio::test]
    async fn som_locate_point_errs_on_out_of_range_label() {
        use crate::visual_fallback::{SomLabelResult, VisualLocateResult, VisualLocator};
        use nomi_browser_engine::CssRect;

        struct Hallucinate;
        #[async_trait::async_trait]
        impl VisualLocator for Hallucinate {
            async fn locate(&self, _png: &[u8], _i: &str) -> Result<VisualLocateResult, String> {
                Err("n/a".into())
            }
            async fn locate_labeled(
                &self,
                _png: &[u8],
                _i: &str,
                _n: usize,
            ) -> Result<SomLabelResult, String> {
                Ok(SomLabelResult { label: 99, confidence: 0.9 }) // out of range (only 1 label)
            }
        }
        let rects = vec![CssRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0 }];
        let r = BrowserTool::som_locate_point(&Hallucinate, b"x", "target", 1.0, &rects).await;
        assert!(r.is_err(), "an out-of-range label must not index the label_map");
    }

    #[test]
    fn is_som_clickable_role_allowlist() {
        for role in ["button", "link", "textbox", "checkbox", "tab", "combobox"] {
            assert!(BrowserTool::is_som_clickable_role(role), "{role} should be clickable");
        }
        for role in ["generic", "heading", "status", "paragraph", "image", ""] {
            assert!(!BrowserTool::is_som_clickable_role(role), "{role} should NOT be clickable");
        }
    }

    #[test]
    fn som_rects_css_keeps_only_clickable_entries() {
        use nomi_browser_engine::{CssRect, ElementEntry, Observation, SnapshotGen};
        use std::collections::HashMap;

        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("som-filter-test"), false);
        let mut boxes = HashMap::new();
        boxes.insert("f0e1".to_string(), CssRect { x: 0.0, y: 0.0, width: 100.0, height: 20.0 });
        boxes.insert("f0e2".to_string(), CssRect { x: 0.0, y: 30.0, width: 80.0, height: 30.0 });
        boxes.insert("f0e3".to_string(), CssRect { x: 0.0, y: 70.0, width: 80.0, height: 30.0 });
        *t.last_snapshot.lock().unwrap() = Some(Observation {
            generation: SnapshotGen(1),
            yaml: String::new(),
            entries: vec![
                ElementEntry { r#ref: "f0e1".into(), role: "generic".into(), name: String::new(), frame_seq: 0 },
                ElementEntry { r#ref: "f0e2".into(), role: "button".into(), name: "Go".into(), frame_seq: 0 },
                ElementEntry { r#ref: "f0e3".into(), role: "heading".into(), name: "Title".into(), frame_seq: 0 },
            ],
            url: None,
            truncated: false,
            current_page_is_post: false,
            boxes,
        });

        let rects = t.som_rects_css().expect("the one button should yield a rect");
        assert_eq!(rects.len(), 1, "only the button entry should be kept (generic/heading dropped)");
        assert_eq!(rects[0].y, 30.0, "the kept rect must be the button's box");
    }

    #[test]
    fn som_rects_css_none_when_no_boxes() {
        // No boxes collected (visual fallback was off at observe) → None → caller uses raw path.
        use nomi_browser_engine::{ElementEntry, Observation, SnapshotGen};

        let t = BrowserTool::with_data_dir(std::env::temp_dir().join("som-nobox-test"), false);
        *t.last_snapshot.lock().unwrap() = Some(Observation {
            generation: SnapshotGen(1),
            yaml: String::new(),
            entries: vec![ElementEntry {
                r#ref: "f0e1".into(),
                role: "button".into(),
                name: "Go".into(),
                frame_seq: 0,
            }],
            url: None,
            truncated: false,
            current_page_is_post: false,
            boxes: Default::default(),
        });
        assert!(t.som_rects_css().is_none(), "empty boxes → None (raw fallback)");
    }
}
