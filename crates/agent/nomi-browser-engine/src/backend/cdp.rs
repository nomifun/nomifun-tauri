//! [`CdpBackend`]：方案 A 的 P0 浏览器后端。
//!
//! **铁律（spike 锁定）**：用 [`crate::transport::Connection`] 发**裸 CDP 命令**，
//! **绝不**碰 chromiumoxide 高层 `Browser`/`Page`。CDP 命令的**生成类型**（params/returns）
//! 经 `chromiumoxide::cdp::*` re-export 复用——那只是 serde 结构体，不是高层 API。
//!
//! 持有物（缺一不可）：
//! - [`Connection`]：已 connect + 起 attach loop + enable_auto_attach 的传输。
//! - `page_session`：一个 page target 的 sessionId（经 createTarget + attachedToTarget 取到），
//!   后续 navigate/screenshot 都发到它。
//! - `child`：托管的 chrome 进程句柄——**保活**，Drop 即清理整棵进程树（Builder 的
//!   kill_on_drop + 三平台清理网）。绝不能提前 drop，否则 chrome 残留。
//! - `_attach_loop`：attach 处理循环的 JoinHandle，保活让子 session 持续被登记。
//! - capabilities 快照（headful/display）。
//!
//! 错误映射（Task B 故意把 `TransportError` 与 `BrowserError` 解耦，留在此处映射）：
//! 见 [`map_transport_err`]。绝不 panic。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::browser::{
    CancelDownloadParams, EventDownloadProgress, EventDownloadWillBegin, SetDownloadBehaviorBehavior,
    SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    EnableParams as DomEnableParams, GetFrameOwnerParams, ResolveNodeParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams as FetchEnableParams, EventRequestPaused, FailRequestParams,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams as NetworkEnableParams, ErrorReason as NetworkErrorReason, ResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, CaptureScreenshotReturns,
    EnableParams as PageEnableParams, NavigateParams, NavigateReturns, PrintToPdfParams,
    PrintToPdfReturns,
};
use chromiumoxide::cdp::browser_protocol::storage::{
    GetCookiesParams as StorageGetCookiesParams, SetCookiesParams as StorageSetCookiesParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    CreateTargetParams, EventAttachedToTarget,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallArgument, CallFunctionOnParams, EvaluateParams, ExecutionContextId, RemoteObjectId,
};
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;

use crate::aria_ref::{frame_prefix, RefRecord, RefTable};
use crate::actions::{ActResult, Effect};
use crate::engine::{
    BrowserEngine, BrowserError, Capabilities, ElementEntry, LoadState, NavResult, Observation,
    ObserveOpts,
};
use crate::injected::{InjectError, InjectionManager};
use crate::launch::{launch_chrome, LaunchConfig, Launched};
use crate::nav::{
    self, InflightCounter, LifecycleSignal, NavSettleState, NETWORK_IDLE_CAP, NETWORK_IDLE_QUIET,
    SETTLE_QUIET, SPA_SETTLE_TIMEOUT,
};
use crate::observe::{stitch, FrameSnapshot};
use crate::progress::{AbortReason, Progress};
use crate::redact;
use crate::tabs::{OopifEntry, TabHandles, TabRecord};
use crate::transport::{Connection, TransportError, ROOT_SESSION};

/// 拿到新 page 的 `attachedToTarget` 事件的上限（flatten auto-attach 通常 <1s）。
const PAGE_ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

/// observe 时等主帧 utility-world context 物化的上限（fresh navigate 后 world 创建有延迟；
/// 通常 <500ms）。超时 → `NavFailed{kind:"context"}`（调用方可短重试）。
const OBSERVE_CONTEXT_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 把传输/会话层错误映射到引擎错误。**绝不 panic**；让模型读到可路由的语义。
///
/// - `Timeout` → `NavFailed`（多见于 navigate/load 等不来；语义是「这次操作没完成」）。
/// - `Closed` / `SessionClosed` → `SessionLost{recoverable:false}`（连接/页面没了）。
/// - `SessionCrashed` → `SessionLost{recoverable:true}`（标签崩了，可重开新 target 恢复）。
/// - `Cdp{code,message}` → `Other`（浏览器侧拒绝；带上 code/message 供诊断）。
/// - `Protocol` → `Other`（我方序列化/路由不变量问题）。
pub fn map_transport_err(e: TransportError) -> BrowserError {
    match e {
        TransportError::Timeout => BrowserError::NavFailed {
            kind: "cdp command timed out".into(),
        },
        TransportError::Closed | TransportError::SessionClosed => {
            BrowserError::SessionLost { recoverable: false }
        }
        TransportError::SessionCrashed => BrowserError::SessionLost { recoverable: true },
        TransportError::Cdp { code, message } => {
            BrowserError::Other(format!("cdp error {code}: {message}"))
        }
        TransportError::Protocol(msg) => BrowserError::Other(format!("cdp protocol error: {msg}")),
    }
}

/// 把**注入管线**错误 [`InjectError`] 映射到引擎错误。**绝不 panic**；穷尽 match（不写 `_`，
/// 这样 injected.rs 新增变体时编译期就逼我们补语义，而非静默归入 `Other`）。observe（Task 6）
/// 调 `call_injected` 拿 aria 时用它。
///
/// - `Transport` → 复用 [`map_transport_err`]（底层传输/会话语义不在这里重新分类）。
/// - `ContextNotReady` → `NavFailed{kind:"context"}`（utility world 还没物化/正在导航，语义近
///   「这次没拿到可用上下文」，调用方可短重试后再报）。
/// - `JsException` / `Protocol` → `Other`（页面侧 JS 抛异常 / CDP 回包形状异常——都带原文供诊断）。
// observe（Task 6）在 call_injected / 帧路由边界把 InjectError 翻成 BrowserError 时调它。
pub(crate) fn map_inject_err(e: InjectError) -> BrowserError {
    match e {
        InjectError::Transport(t) => map_transport_err(t),
        InjectError::ContextNotReady { .. } => BrowserError::NavFailed {
            kind: "context".into(),
        },
        InjectError::JsException(m) => BrowserError::Other(m),
        InjectError::Protocol(m) => BrowserError::Other(m),
    }
}

/// P0 浏览器后端：自建 transport 发裸 CDP 命令（无 chromiumoxide 高层）。
///
/// **P2 D1 结构改造（DESIGN §13 裁决⑥）**：原先「单 tab 的 per-tab 字段直挂」改为 tab 注册表 +
/// active_target 指针。per-tab 状态（page session / 注入管线 / OOPIF 表 / ref 表 / 主帧 id）下放进
/// [`TabRecord`]，存进 `tabs: HashMap<targetId, TabRecord>`；`active_target` 指向当前作用的 tab。
/// observe/act/navigate 经 [`Self::active_tab_handles`] 短暂锁 `tabs`+`active_target` 克隆出句柄、
/// **立即释放 `tabs` 锁**后用克隆句柄操作（不跨 await 持 `tabs` 锁——见 [`crate::tabs`] 模块级锁设计）。
///
/// **单 tab 场景**（D1 恒态；多 tab 填充是 D3）：`tabs` 恒只 1 项、`active_target` 指向它——行为与
/// 改造前完全一致。
pub struct CdpBackend {
    conn: Connection,
    /// **tab 注册表**：targetId → [`TabRecord`]（吸收原 per-tab 字段）。短暂锁、克隆句柄、立即释放
    /// （绝不跨 await 持有；见 [`crate::tabs`]）。**D3**：`Arc` 包裹——tab 发现循环（`'static` 后台任务）
    /// 持一份克隆，发现新顶层 page 时锁它插入新 [`TabRecord`]（与 observe/act 的短临界区共存，互不跨
    /// await 持锁）。
    tabs: Arc<AsyncMutex<HashMap<String, TabRecord>>>,
    /// **active_target 指针**：当前 observe/act/navigate 默认作用的 tab 的 targetId（DESIGN §13）。
    /// 指向不存在的 tab → [`Self::active_tab_handles`] 返 `BrowserError`（绝不 panic）。**D3**：`Arc`
    /// 包裹——switch/close 改它做逻辑指针切换；发现循环**不**改它（新 tab 不抢焦点）。
    active_target: Arc<AsyncMutex<String>>,
    /// **active_frame 指针**（D4 switch_frame，DESIGN §13）：`Some((session_id, frame_id))` = 已 switch_frame
    /// 切入某 iframe（**页面级动作**默认作用于它）；`None`（默认）= 主帧/顶层（页面级动作作用主文档）。
    /// switch_frame 改它；切 tab 后旧指针对不上当前 active tab → [`Self::active_page_frame`] 退主帧。
    /// **ref-based 动作不受它影响**（按 ref 所属帧路由，本就跨帧）。`Arc`：与其它共享态一致，短临界区锁。
    active_frame: Arc<AsyncMutex<Option<(String, String)>>>,
    /// **act 的 objectGroup 序号源**（C1，DESIGN §7）：每次 `act` `fetch_add(1)` 取一个唯一 `seq`，
    /// 拼成本动作的 objectGroup `act-<seq>`（[`crate::actionability::act_object_group`]）。唯一性保
    /// 证并发/连续动作的句柄组互不串味、各自 `releaseObjectGroup` 不误伤他组。`Relaxed` 足够——只
    /// 需单调唯一，不依赖与其它内存操作的顺序。**保留在 backend（全局即可，非 per-tab）**。
    act_seq: std::sync::atomic::AtomicU64,
    /// 托管的 chrome 进程句柄——保活，Drop 即清理。`AsyncMutex` 仅为内部可变（取 child 杀进程）；
    /// 正常路径靠 kill_on_drop。
    _child: AsyncMutex<Child>,
    /// attach 处理循环句柄——保活，让 flatten 自动附着的子 session 持续被登记。
    _attach_loop: tokio::task::JoinHandle<()>,
    /// **tab 发现后台循环句柄**（D3）——保活。订阅新顶层 page 的 `Target.attachedToTarget`，arm 成
    /// [`TabRecord`] 入 `tabs`（不抢焦点）。backend Drop 即连带 abort（连接随之关闭，循环也会自然退出）。
    _tab_discovery_loop: tokio::task::JoinHandle<()>,
    /// **下载事件后台循环句柄**（E4）——保活。仅当 `setDownloadBehavior` 沙箱已挂（`download_dir`
    /// 为 `Some`）时存在。订阅 `Browser.downloadProgress`，对完成（`state=="completed"`）的下载在其
    /// `filePath` 上打 Win MOTW（`Zone.Identifier` ADS）。mac/linux 为空实现。**绝不**自动打开文件。
    /// backend Drop 即连带 abort。
    _download_loop: Option<tokio::task::JoinHandle<()>>,
    /// **隔离下载目录的绝对路径**（E4 沙箱落点 / F-actions `download`/`save_as_pdf` 落点）。
    /// = [`crate::download::ensure_download_dir`] 的产物（`<per-pet workspace 或 data_dir>/downloads`，
    /// **绝不**用户真实 Downloads）。`Some` 当且仅当下载沙箱已挂（与 `_download_loop` 同生）。
    /// `download`（注入 `<a download>` 触发）的产物落这里、`save_as_pdf`（`Page.printToPDF`）也写这里——
    /// 二者复用 E4 的同一隔离目录（denylist 红线 / MOTW 由 `_download_loop` 在落盘事件上统一施加）。
    /// `None`（无沙箱：纯引擎冒烟）→ `save_as_pdf` 报 `Unsupported`（无落点）、`download` 仍触发但落
    /// chrome 默认行为（无沙箱时本就不该用）。
    download_dir: Option<String>,
    /// **SD-2 上传路径沙箱根**（per-pet 隔离 workspace 目录）。`act_upload_file` 在调
    /// `DOM.setFileInputFiles` 前逐路径 canonicalize + 包含判定：不在此目录下 ⇒
    /// `BrowserError::Blocked`（fail-closed）。`None`（无 per-pet 上下文，如纯引擎冒烟）⇒
    /// **一律拒绝上传**（fail-closed，default-deny）。
    workspace_dir: Option<PathBuf>,
    /// **出口防火墙后台循环句柄**（E5）——保活。对**每个** session（根 browser / page / OOPIF /
    /// **service_worker**）挂 `Fetch.enable` 全流量拦截，订阅 `Fetch.requestPaused`，经
    /// [`crate::firewall::decide`] 判定后 `continueRequest` 放行 / `failRequest` 阻断（IP 封禁硬阻）/
    /// （F1）升审批（跨域 POST-body）。**SW 必须也拦**（裁决⑪/不变量⑬）——P0 保持 SW attach，本循环
    /// 对其 session 也 Fetch.enable。backend Drop 即连带 abort。
    _firewall_loop: tokio::task::JoinHandle<()>,
    /// **P3-G1：注入的出口防火墙配置快照**（裁决①）。= `EngineConfig.firewall`（经 build_backend →
    /// from_launched 透传），**与 `_firewall_loop` 持有的同一份配置**。仅供测试 accessor
    /// [`Self::firewall_config_for_test`] 读回断言「注入值真的到达引擎」（loop 在另一线程内消费，
    /// 无法直接观测）。**P3-D1 后 `FirewallConfig` 不再 `Copy`（改 `Clone`，因加了 `Vec` 域名策略字段）**，
    /// 存一份快照（`.clone()`，零热路径成本）。产品路径不读它（loop 才是真消费者）。
    firewall_config: crate::firewall::FirewallConfig,
    /// **P3-D2：per-session 已批准出口域集合**（决策3 always_allow）。与 `_firewall_loop` 持有的
    /// 同一份（`Arc<Mutex<…>>` 共享）：审批一条被门控出口请求时若选「记住此域」（`EgressVerdict::
    /// ContinueAndRemember`），目标 eTLD+1 记进这里 → 同域后续出口请求不再悬挂审批直接放行。engine
    /// 生命周期内有效（非持久——持久域策略走 `FirewallConfig.allow_etld1` 的 secret 真值，X2）。
    /// backend 持有它仅为保活 + 测试 accessor；真消费者是 loop 的 spawn 审批任务。
    #[allow(dead_code)]
    approved_domains: crate::firewall::ApprovedDomains,
    /// **E3 evaluate 门控配置**（DESIGN §16「evaluate」/ 裁决⑨）：默认 [`crate::evaluate::EvaluateGate::default`]
    /// = **evaluate OFF**（`full_power=false`，default-deny）。act(Evaluate) 经 [`crate::evaluate::gate`]
    /// 据此判放行——**只看全权开关，绝不看 session_mode**（yolo/companion 无从豁免；不变量⑧）。
    /// **LIVE 读接线在 services.rs / F 阶段**（`read_bool_pref` 范式从 client_preferences 读全权开关灌进
    /// 来，使切换无需重启）；E3 引擎层先持默认 OFF 的门 + 纯逻辑放行判定。`persistent_login` 占位 false
    /// 待 P6（互斥逻辑已就位）。`AsyncMutex` 与其它共享态一致（F1 可在每次 act 前更新 LIVE 值）。
    evaluate_gate: AsyncMutex<crate::evaluate::EvaluateGate>,
    /// capabilities 快照。
    headful: bool,
    display_available: bool,
    /// **引擎级 observe⊥act 串行门**（DESIGN §22「observe 与 act 互斥」+「per-target act 串行」）。
    /// 跨 `navigate`/`screenshot`/`observe`/`act` 整个方法体持有：快照（observe）绝不与改 DOM 的动作
    /// 在同一引擎上交错（否则给模型陈旧 ref / 半应用页）。此前该不变量仅靠调用方串行成立
    /// （`is_concurrency_safe==false` → orchestrator partition + 网关 `CompanionBrowser::lock`）；现在
    /// **引擎自身**保证——并发调用方也无法交错 observe/act。公平 `tokio::sync::Mutex`，只在单次已被
    /// 截止时间界定的操作内持有（每 CDP 命令超时 + `Progress` 截止 / `ACT_TIMEOUT`），绝不跨无界等待 → 不死锁。
    /// **作用域 per-engine**（= per Chrome 进程）：DESIGN §22「per-BrowserContext 可并发」由上层实现——
    /// 不同引擎（网关 per-companion 各自 Chrome 进程）持不同 op_mutex 并行（`BrowserRegistry::execute_parallel`），
    /// 此锁绝不跨引擎。重入安全：`navigate`/`screenshot`/`observe`/`act` 体内均只调 `*_impl`/`*_on_session`
    /// 助手、绝不回调这四个 trait 方法（已对抗式 grep 校验），故非重入锁不会自死锁。
    op_mutex: AsyncMutex<()>,
    /// Known-secret exact-blackout registry (shared with facade via `Arc`). Debug serializers
    /// read this set and `String::replace` each value with `[KNOWN_SECRET_REDACTED]` before
    /// heuristic redaction passes. See [`crate::KnownSecretValues`] doc for invariants.
    known_secret_values: crate::KnownSecretValues,
}

impl CdpBackend {
    /// 用一次成功的 [`launch_chrome`] 产物建后端：connect → 起 attach loop →
    /// enable_auto_attach → 取一个 page session。
    ///
    /// **编排铁律（Task B 约定）**：先 `run_attach_loop()`（装监听）再
    /// `enable_auto_attach()`（放行），否则首批子 session 的 attach 事件会丢。
    ///
    /// `#[allow(clippy::too_many_arguments)]`：本构造器逐参注入引擎配置（download/evaluate/firewall/
    /// egress 等都是 P3 各阶段一路打通的注入链真值，调用点仅 build_backend + 测试 helper），
    /// 折成 config 结构会牵动 G1/D1/D2 多个已交付调用点的同步改动——超出本次重构范围。
    #[allow(clippy::too_many_arguments)]
    pub async fn from_launched(
        launched: Launched,
        headful: bool,
        display_available: bool,
        download_dir: Option<String>,
        workspace_dir: Option<PathBuf>,
        evaluate_full_power: bool,
        evaluate_persistent_login: bool,
        firewall: crate::firewall::FirewallConfig,
        egress_approver: Option<Arc<dyn crate::firewall::EgressApprover>>,
        storage_state: Option<serde_json::Value>,
        known_secret_values: crate::KnownSecretValues,
        // SD-1: DNS→IP SSRF guard 的可注入 resolver。`None` → 生产默认 [`TokioResolver`]（真实 DNS）;
        // 测试注入 fake 映射(host→固定 IP)以完全隔离真实网络、并能精确验「公网域放行→抵 approver /
        // 私网域先于 approver 被 SSRF 守卫 Block」这一关键交互(seam 此前硬编码,测试无从覆盖)。
        dns_resolver: Option<Arc<dyn crate::firewall::HostResolver>>,
    ) -> Result<Self, BrowserError> {
        let Launched { child, transport } = launched;

        let conn = Connection::connect_launched(transport)
            .await
            .map_err(map_transport_err)?;

        // 先装 attach loop（订阅在循环内部），再放行自动附着。顺序勿换。
        let attach_loop = conn.run_attach_loop();
        conn.enable_auto_attach().await.map_err(map_transport_err)?;

        // 取一个 page session（createTarget + 等其 attachedToTarget）。D1：需要 targetId（tabs 的 key
        // + active_target 指针），故 create_page_session 返 (target_id, session_id)。
        let (page_target_id, page_session) =
            create_page_session(&conn).await?;

        // E4 下载沙箱：在**根 browser session** 挂 setDownloadBehavior（browser-level，作用全 context）
        // + 起下载事件循环（完成后打 MOTW）。仅当传入了隔离 download_dir。先订阅事件再放行行为不是
        // 必须（downloadProgress 在下载真正发生时才来，启动期挂好即可），但本循环在 setDownloadBehavior
        // 之前 spawn，确保不漏首个下载事件。失败 best-effort（warn 不致命）——沙箱缺失只降级到无 MOTW，
        // 不应阻断引擎创建；但若上层要求严格隔离，缺失即风险，故 warn 留痕。
        let download_loop = if let Some(ref dir) = download_dir {
            let h = spawn_download_loop(conn.clone());
            if let Err(e) = set_download_behavior_sandbox(&conn, dir).await {
                tracing::warn!(error = %e, dir = %dir, "setDownloadBehavior sandbox failed; downloads may fall back to chrome default");
            }
            Some(h)
        } else {
            None
        };

        // 取一个 page session（createTarget + 等其 attachedToTarget）。D1：需要 targetId（tabs 的 key
        // + active_target 指针），故 create_page_session 返 (target_id, session_id)。
        // D3：捕获初始主 page session，供发现循环与之区分（主 page 已单独 arm，勿重 arm）。
        let main_page_session_for_discovery = page_session.clone();

        // D3：把「为一个 (target_id, session_id) arm injection + inject loop + oopif loop + 建 TabRecord」
        // 抽成可复用的 arm_tab helper。初始 tab 与发现循环里的新 tab 都用它（同一套 arm 逻辑，零分叉）。
        let initial_tab =
            arm_tab(&conn, &page_target_id, &page_session).await?;

        // D1/D3：把初始 page 作为**唯一一个 TabRecord** 插入 tabs，active_target 指向它。
        // 单 tab 场景：tabs 永远只 1 项、active 指向它——行为与改造前完全一致。
        let mut tabs_map = HashMap::new();
        tabs_map.insert(page_target_id.clone(), initial_tab);
        let tabs = Arc::new(AsyncMutex::new(tabs_map));
        let active_target = Arc::new(AsyncMutex::new(page_target_id));

        // D3：起 tab 发现后台循环——订阅新顶层 page 的 attachedToTarget，arm 成 TabRecord 入 tabs
        // （**不抢焦点、不改 active**）。捕获初始主 page session 以与之区分（主 page 已单独 arm，勿重 arm）。
        let tab_discovery_loop = spawn_tab_discovery_loop(
            conn.clone(),
            main_page_session_for_discovery,
            tabs.clone(),
        );

        // E5 出口防火墙：对**每个** session（根 browser / page / OOPIF / **service_worker**）挂
        // `Fetch.enable` 全流量拦截 + 订阅 `Fetch.requestPaused` 判定放行/阻断。**SW 必须也拦**
        // （裁决⑪/不变量⑬）——P0 保持 SW attach（transport.rs handle_attached 不 detach），本循环
        // 对其 session 也 Fetch.enable。**P3-G1（裁决①）**：firewall 配置从硬编码 `FirewallConfig::default()`
        // 改为**接收注入值**（`EngineConfig.firewall` 经 build_backend → from_launched 透传）——默认仍是
        // default（IP 封禁开 + 跨域 POST 门控检测开），但上层（D1）可注入 per-pet 域名 allowlist 真值。
        // 先 spawn 循环（订阅 attachedToTarget + requestPaused），再对**已附着**的根 + 初始 page session
        // 补挂 Fetch.enable（新 session 由循环里的 attachedToTarget 处理）。
        // **P3-D1**：`FirewallConfig` 加 `Vec` 域名策略字段后**不再 Copy（改 Clone）**——故存快照用
        // `.clone()`，再把配置 move 进 loop（loop 拿走的与快照同值）。快照供测试 accessor 读回断言注入生效。
        let firewall_config = firewall.clone();
        // P3-D2：per-session「记住此域」已批准出口域集合（决策3 always_allow）。backend 持一份（engine
        // 生命周期内有效），loop 拿一份克隆（共享同一 Arc<Mutex<…>>）——审批批准并「记住」后，同域后续
        // 出口直接放行。
        let approved_domains = crate::firewall::ApprovedDomains::new();
        // SD-1: DNS→IP SSRF guard — resolver（注入式：`None`=生产默认 TokioResolver 走真实 DNS；
        // 测试注入 fake 映射,完全隔离真实网络）+ 缓存（注入防火墙循环）。
        let dns_resolver: Arc<dyn crate::firewall::HostResolver> =
            dns_resolver.unwrap_or_else(|| Arc::new(crate::firewall::TokioResolver::default()));
        let dns_cache = crate::firewall::DnsResolverCache::default();
        let firewall_loop = spawn_fetch_firewall_loop(
            conn.clone(),
            firewall,
            egress_approver,
            approved_domains.clone(),
            dns_resolver,
            dns_cache,
        );
        // 已附着 session 补挂（循环只覆盖**之后**新 attach 的；启动时已在的根 + 初始 page 在此补）。
        if let Err(e) = enable_fetch_on_session(&conn, ROOT_SESSION).await {
            tracing::warn!(error = %e, "Fetch.enable on root browser session failed; egress firewall degraded");
        }
        if let Err(e) = enable_fetch_on_session(&conn, &page_session).await {
            tracing::warn!(error = %e, "Fetch.enable on initial page session failed; egress firewall degraded");
        }
        // F1-sec (I1 启动竞态收口)：防火墙循环在 `enable_auto_attach` **之后**才 subscribe
        // `attachedToTarget`，故启动瞬间已 attach 的 **service_worker** 的 attach 事件可能早于订阅丢失
        // → 那个 SW 漏挂 Fetch.enable，其出口请求绕过防火墙（裁决⑪/不变量⑬：SW 必须也拦）。但更早启动的
        // attach loop 已把这些 SW session 登记进注册表（P0 保持 SW attach 不 detach），故据 target_type
        // 枚举已存在的 service_worker session 补挂 Fetch.enable（与上面对根/page 的补挂同理，收口竞态）。
        for sw_session in conn.registry().session_ids_of_type("service_worker") {
            if let Err(e) = enable_fetch_on_session(&conn, &sw_session).await {
                tracing::warn!(
                    error = %e, session_id = %sw_session,
                    "Fetch.enable on a pre-existing service_worker session failed; egress firewall has a gap for that SW"
                );
            } else {
                tracing::debug!(
                    session_id = %sw_session,
                    "Fetch.enable armed on a pre-existing service_worker session (startup-race close)"
                );
            }
        }

        let backend = Self {
            conn,
            tabs,
            active_target,
            active_frame: Arc::new(AsyncMutex::new(None)),
            act_seq: std::sync::atomic::AtomicU64::new(0),
            _child: AsyncMutex::new(child),
            _attach_loop: attach_loop,
            _tab_discovery_loop: tab_discovery_loop,
            _download_loop: download_loop,
            // F-actions：保留隔离下载目录绝对路径，供 download（触发落点验证）/ save_as_pdf（printToPDF
            // 写入）复用 E4 沙箱的同一目录。与 _download_loop 同生（仅当沙箱已挂）。
            download_dir,
            // SD-2：上传路径沙箱根（per-pet workspace）。act_upload_file 逐路径 canonicalize + 包含判定。
            workspace_dir,
            _firewall_loop: firewall_loop,
            // P3-G1：保留注入的 firewall 快照（与 loop 同值）供测试读回断言注入生效。
            firewall_config,
            // P3-D2：保留 always_allow 已批准域集合（与 loop 同 Arc）。
            approved_domains,
            // E3 evaluate 门控：full_power 由上层 LIVE 读（client_preferences）经 EngineConfig 灌入
            // （默认 false = default-deny）。SD-6: persistent_login 同范式 LIVE 灌入（默认 false = default-deny
            // 基线；产品 ON 由 factory host_default=true 实现）。
            evaluate_gate: AsyncMutex::new(crate::evaluate::EvaluateGate {
                full_power: evaluate_full_power,
                persistent_login: evaluate_persistent_login,
            }),
            headful,
            display_available,
            // 引擎级 observe⊥act 串行门（见字段 doc）。每引擎一把,初始空闲。
            op_mutex: AsyncMutex::new(()),
            // Known-secret blackout: store the shared Arc for debug serializers to read.
            known_secret_values,
        };

        // ── W4d 持久登录：启动注入 storage_state（灌登录态）──────────────────────────
        // `EngineConfig.storage_state`（G1 预留的 `Option<Value>`，经 build_backend 透传）= 上层从
        // per-pet vault（[`crate::vault::load_storage_state`]）解密读出的跨会话登录态。`Some` → 解析成
        // [`crate::storage_state::StorageState`] → **此刻 page/tab/context 已建好**，把 cookie 灌进本引擎
        // 的默认 browser context（`restore_cookies`，**context-bound：无需先 navigate 即生效**——这是持久登录
        // 在「navigate 之前」就恢复会话身份的关键）+ 把当前页面 origin 的 localStorage 灌回
        // （`restore_local_storage`，**origin-bound**：启动时 active tab 停在 about:blank，无 origin 匹配
        // → no-op，待 caller navigate 到目标 origin 后由上层再调 restore_local_storage 补灌——见方法 doc）。
        // **`None`（默认）→ 完全不碰（现行为零回归）**。注入失败 best-effort（warn 不致命）：登录态恢复是
        // 增强，灌失败应降级到「未登录」起点（用户重登）而非阻断引擎启动。
        if let Some(value) = storage_state {
            match crate::storage_state::StorageState::from_json(value) {
                Ok(state) => {
                    if let Err(e) = backend.restore_cookies(&state).await {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            error = %e,
                            "W4d persistent-login: restore_cookies on startup failed (degrading to no persisted login)"
                        );
                    }
                    // localStorage origin-bound：启动时 about:blank 无匹配 origin → 多为 no-op；仍调一次以
                    // 覆盖「引擎构造前已 navigate」的边角（当前 from_launched 不预导航，故实际 no-op）。
                    if let Err(e) = backend.restore_local_storage(&state).await {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            error = %e,
                            "W4d persistent-login: restore_local_storage on startup failed (degrading)"
                        );
                    }
                }
                Err(e) => {
                    // storage_state Value 形态不对（不是合法 StorageState JSON）→ 优雅跳过（不阻断启动）。
                    tracing::warn!(
                        target: "nomi_browser_engine::backend::cdp",
                        error = %e,
                        "W4d persistent-login: EngineConfig.storage_state is not a valid StorageState (skipping inject)"
                    );
                }
            }
        }

        Ok(backend)
    }
}

impl CdpBackend {
    /// **active tab 句柄快照**（D1 锁模式核心；DESIGN §13 裁决⑥ + [`crate::tabs`] 锁设计）：短暂锁
    /// `active_target` + `tabs`，从 active [`TabRecord`] **克隆出**所有可独立持有的句柄
    /// （[`TabHandles`]），**立即释放两把锁**后返回。observe/act/navigate 全程用克隆出的句柄操作——
    /// **绝不**跨 await 持 `tabs` 锁（否则阻塞 D3 tab 发现循环 + observe 内嵌套锁 ref_table 死锁）。
    ///
    /// `active_target` 指向的 tab 不在 `tabs` 里（D3 close 后的竞态 / 内部不变量破坏）→ 返
    /// [`BrowserError::SessionLost`]`{recoverable:false}`（绝不 panic）。**单 tab 场景**：tabs 恒 1 项、
    /// active 指向它，必命中。
    pub(crate) async fn active_tab_handles(&self) -> Result<TabHandles, BrowserError> {
        // 先锁 active_target 取指针（短临界区，clone 出 String 即放）。
        let target_id = self.active_target.lock().await.clone();
        // 再锁 tabs 取 active 记录、克隆句柄、立即释放（不跨任何 await 持 tabs 锁）。
        let guard = self.tabs.lock().await;
        let tab = guard.get(&target_id).ok_or(BrowserError::SessionLost {
            recoverable: false,
        })?;
        Ok(TabHandles {
            target_id: tab.target_id.clone(),
            session_id: tab.session_id.clone(),
            // InjectionManager: Clone（共享 Arc 缓存，不复制后台循环）。
            injection: tab.injection.clone(),
            main_frame_id: tab.main_frame_id.clone(),
            // Arc clone：锁外独立锁这两个（observe 跨多 await 不持 tabs 锁）。
            oopif_managers: tab.oopif_managers.clone(),
            ref_table: tab.ref_table.clone(),
            debug: tab.debug.clone(),
        })
        // guard 在此 drop（释放 tabs 锁）。
    }

    /// **Takeover seam: bring the headful browser window to the foreground.**
    ///
    /// - Headful + display available → sends `Page.bringToFront` (CDP) on the active
    ///   page session + `Target.activateTarget` on the browser session. Returns `Ok(())`.
    /// - Headless or no display → returns `Err(BrowserError::Unsupported)` with
    ///   capability="takeover" so the caller can map it to [`TakeoverResolution::Unavailable`].
    ///
    /// Does NOT hold `op_mutex` — this is a pure window-management command that does
    /// not interact with observe/act serialization (mirrors `activateTarget` in switch_tab).
    pub async fn bring_to_front(&self) -> Result<(), BrowserError> {
        if !self.headful || !self.display_available {
            return Err(BrowserError::Unsupported {
                capability: "takeover".into(),
                hint: "headful window required but engine is headless or no display available"
                    .into(),
            });
        }

        // Get the active page session.
        let handles = self.active_tab_handles().await?;
        let session = handles.session_id.as_str();

        // Page.bringToFront — brings the page to front (activates the tab in the window
        // and focuses the window). This is the primary CDP command for foregrounding.
        use chromiumoxide::cdp::browser_protocol::page::BringToFrontParams;
        let _ = self
            .conn
            .send::<BringToFrontParams>(session, &BringToFrontParams::default())
            .await
            .map_err(map_transport_err)?;

        // Also activate the target at the browser level (best-effort, like switch_tab).
        use chromiumoxide::cdp::browser_protocol::target::ActivateTargetParams;
        let _ = self
            .conn
            .send::<ActivateTargetParams>(
                ROOT_SESSION,
                &ActivateTargetParams::new(handles.target_id.clone()),
            )
            .await;

        Ok(())
    }
}

/// **arm 一个 tab（D3 复用核心）**：为给定 `(target_id, session_id)` 物化注入管线（utility world +
/// 现存帧补建 + context 登记循环）、读权威主帧 id、接 OOPIF arm 循环，建好一个完整的 [`TabRecord`]。
///
/// 初始 tab（[`CdpBackend::from_launched`]）与发现循环里的新顶层 page（[`spawn_tab_discovery_loop`]）
/// **共用本 helper**——同一套 arm 逻辑，零分叉。返回的 [`TabRecord`] 持有 `_inject_loop`/`_oopif_loop`
/// 两个后台 `JoinHandle`：它们订阅**全局共享连接**的 broadcast，靠 `RecvError::Closed` 退出——但**连接
/// 关单个 tab 时仍存活**，故关 tab 仅从 `tabs` 移除 TabRecord（drop 这俩 handle）**不会**让循环退出
/// （drop 是 detach 非 abort）。**close_tab 必须显式 `.abort()` 这俩 handle**（见 [`CdpBackend::close_tab_impl`]）。
///
/// 错误：injection arm / 读主帧失败 → 映射为 [`BrowserError`]（绝不 panic）。
async fn arm_tab(
    conn: &Connection,
    target_id: &str,
    session_id: &str,
) -> Result<TabRecord, BrowserError> {
    // page session 注入管线：new + arm（物化 utility world + 现存帧补建 + 起 context 登记循环）。
    // 保活其 loop 句柄，否则 world 创建事件不再被收下。
    let injection = InjectionManager::new(conn.clone(), session_id.to_string());
    let inject_loop = injection.arm().await.map_err(map_inject_err)?;

    // 主 frameId = page target 的 targetId（CDP 约定）。从 page session 的 frameTree 读权威主帧 id
    // （与 targetId 一致，但不依赖外部传入）。
    let main_frame_id = injection.main_frame_id().await.map_err(map_inject_err)?;

    // OOPIF 子 session arm 接线骨架：后台订阅 attachedToTarget，对 iframe 类型的子 session（非本 page
    // session）arm 一个 InjectionManager 入 oopif_managers。真跨源 OOPIF 须 http fixture 后续验
    // （见 `TODO(verify-oopif)`）。每 tab 自有一份 oopif_managers + 一条 arm 循环（per-tab 隔离）。
    let oopif_managers: std::sync::Arc<AsyncMutex<HashMap<String, OopifEntry>>> =
        std::sync::Arc::new(AsyncMutex::new(HashMap::new()));
    let oopif_loop =
        spawn_oopif_arm_loop(conn.clone(), session_id.to_string(), oopif_managers.clone());

    // 调试捕获：enable Runtime/Log + 起长驻 drain 循环写入有界缓冲。
    // Network.enable 在 navigate 内已幂等调用，此处也 enable 确保非导航期事件亦被捕获。
    let debug_buffers = std::sync::Arc::new(std::sync::Mutex::new(
        crate::debug_capture::DebugBuffers::default(),
    ));
    let debug_loop = spawn_debug_capture_loop(
        conn.clone(),
        session_id.to_string(),
        debug_buffers.clone(),
    );

    Ok(TabRecord {
        target_id: target_id.to_string(),
        session_id: session_id.to_string(),
        injection,
        _inject_loop: inject_loop,
        main_frame_id,
        oopif_managers,
        _oopif_loop: oopif_loop,
        ref_table: std::sync::Arc::new(AsyncMutex::new(None)),
        debug: debug_buffers,
        _debug_loop: debug_loop,
    })
}

/// **tab 发现后台循环（D3，DESIGN §13 + 裁决⑥/不变量⑮）**：订阅 `Target.attachedToTarget`（全 session
/// 通配），对**新顶层 page**（`type=="page"`，非主 page session，不在 `tabs`）调 [`arm_tab`] 建
/// [`TabRecord`] 入 `tabs`——**不抢焦点、不改 active**（返「新标签已打开[last4]」让 LLM 显式 switch，
/// browser-use 策略 / DESIGN:188）。
///
/// **与 OOPIF arm 循环（[`spawn_oopif_arm_loop`]）的协调防重复 arm**：本循环 [`crate::tabs::should_arm_as_page`]
/// **只收 `type=="page"`**；OOPIF 循环只收 `type=="iframe"`。二者各自筛 type，互不重叠——同一 attach 事件
/// 绝不被两路同时 arm。再加「不在 tabs」守卫（CDP 对同 target 多次 attach 时不重复 arm，`tabs` map 无重复 key）。
///
/// **不等子 session 放行**：`run_attach_loop`（全局 attach loop）已先登记子 session 并放行
/// （runIfWaitingForDebugger）。本循环 arm 前轮询确认子 session 已登记（仿 OOPIF 循环），再物化注入管线。
///
/// 所有错误 best-effort：单个 tab arm 失败只 warn 不影响其它，**绝不 panic**。连接关闭（`RecvError::Closed`）
/// → 退出循环（backend Drop 关连接即触发）。
fn spawn_tab_discovery_loop(
    conn: Connection,
    main_page_session: String,
    tabs: Arc<AsyncMutex<HashMap<String, TabRecord>>>,
) -> tokio::task::JoinHandle<()> {
    use chromiumoxide::cdp::browser_protocol::target::EventAttachedToTarget;
    let mut rx = conn.subscribe(EventAttachedToTarget::IDENTIFIER, None);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let Ok(att) =
                        serde_json::from_value::<EventAttachedToTarget>(ev.params.clone())
                    else {
                        continue;
                    };
                    let sid: String = att.session_id.clone().into();
                    let ttype = att.target_info.r#type.clone();
                    let tid: String = att.target_info.target_id.clone().into();

                    // type 分流（防与 OOPIF 循环重复 arm）+ 非主 session + 不在 tabs（短临界区查后释放锁）。
                    let already = tabs.lock().await.contains_key(&tid);
                    let is_main = sid == main_page_session;
                    if !crate::tabs::should_arm_as_page(&ttype, is_main, already) {
                        continue;
                    }

                    // 等子 session 在注册表登记（run_attach_loop 的 handle_attached 负责登记 + 放行）。
                    let deadline = tokio::time::Instant::now() + OOPIF_SESSION_REGISTER_TIMEOUT;
                    let registry = conn.registry().clone();
                    while !registry.has_session(&sid) {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if !registry.has_session(&sid) {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            session_id = %sid, target_id = %tid,
                            "new page child session never registered; skip arm"
                        );
                        continue;
                    }

                    // arm 成 TabRecord（复用 arm_tab）。失败 best-effort：warn 后继续（不影响已有 tab）。
                    match arm_tab(&conn, &tid, &sid).await {
                        Ok(record) => {
                            // 再次确认未被并发插入（双查，避免两条 attach 事件窗口竞态重 arm）。
                            let mut guard = tabs.lock().await;
                            if guard.contains_key(&tid) {
                                // 已被插入：丢弃本次 record（其 loop handle 随 drop 是 detach，但本 record
                                // 的注入管线没被任何 tab 用，下面显式 abort 它的两个 loop 防泄漏）。
                                record._inject_loop.abort();
                                record._oopif_loop.abort();
                                continue;
                            }
                            let last4 = crate::tabs::last4(&tid);
                            guard.insert(tid.clone(), record);
                            drop(guard);
                            // **不抢焦点、不改 active**：只记日志（LLM 经 tabs/switch_tab 显式切换）。
                            tracing::info!(
                                target: "nomi_browser_engine::backend::cdp",
                                target_id = %tid, last4 = %last4,
                                "新标签已打开[{last4}]（未抢焦点；observe/act 仍在原标签，需显式 switch_tab）"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "nomi_browser_engine::backend::cdp",
                                target_id = %tid, error = %e,
                                "arm discovered tab failed (non-fatal)"
                            );
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}


/// 2. **再** `Target.createTarget{url:"about:blank"}` 拿 targetId；
/// 3. 等订阅里出现 `target_info.target_id == targetId` 的 attach 事件 → 取其 sessionId。
///
/// flatten auto-attach（enable_auto_attach 已开）会自动 attach 新 page，故无需手动 attach。
/// attach loop 会**同时**登记该子 session 并放行；本函数只需拿到 (targetId, sessionId)。
///
/// **D1**：返回 `(target_id, session_id)`——targetId 是 tabs 注册表的 key + active_target 指针
/// （`createTarget` 回包已给 targetId，attach 事件的 `target_info.target_id` 与之一致；二者择一即可，
/// 这里直接复用 createTarget 拿到的 `target_id`）。
async fn create_page_session(
    conn: &Connection,
) -> Result<(String, String), BrowserError> {
    // 1) 先订阅 attach 事件（在 createTarget 之前，避免错过）。
    let mut attached_rx = conn.subscribe(EventAttachedToTarget::IDENTIFIER, None);

    // 2) 在根 session 上建 page target（默认 browser context）。
    let params = CreateTargetParams::new("about:blank");
    let result = conn
        .send::<CreateTargetParams>(ROOT_SESSION, &params)
        .await
        .map_err(map_transport_err)?;
    let target_id: String = result
        .get("targetId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BrowserError::Other("createTarget response missing targetId".into()))?
        .to_string();

    // 3) 等到该 targetId 的 attach 事件，取 sessionId。
    let deadline = tokio::time::Instant::now() + PAGE_ATTACH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(BrowserError::Other(format!(
                "timed out waiting for attachedToTarget of page target {target_id}"
            )));
        }
        match tokio::time::timeout(remaining, attached_rx.recv()).await {
            Ok(Ok(ev)) => {
                // 只认 page 类型且 targetId 匹配的 attach。
                let parsed: Result<EventAttachedToTarget, _> =
                    serde_json::from_value(ev.params.clone());
                if let Ok(att) = parsed {
                    let tid: String = att.target_info.target_id.clone().into();
                    if tid == target_id && att.target_info.r#type == "page" {
                        return Ok((target_id, att.session_id.into()));
                    }
                }
                // 非目标事件（其它 target 的 attach）：继续等。
            }
            // 广播落后（lagged）→ 继续收（可能错过，但下个匹配仍能拿到；超时兜底）。
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            // 连接关闭。
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(BrowserError::SessionLost { recoverable: false });
            }
            // 超时。
            Err(_elapsed) => {
                return Err(BrowserError::Other(format!(
                    "timed out waiting for attachedToTarget of page target {target_id}"
                )));
            }
        }
    }
}

/// OOPIF 子 session 等子 session 已登记的轮询上限（`run_attach_loop` 先登记再放行，但两路
/// 订阅者的调度顺序非确定，故 arm 前轮询确认子 session 已在注册表）。
const OOPIF_SESSION_REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// **OOPIF arm 后台循环（接线骨架）**：订阅 `Target.attachedToTarget`（全 session 通配），
/// 对类型为 **`iframe`** 且**非本 page session** 的子 session arm 一个 [`InjectionManager`]
/// 入 `oopif_managers`。这让跨进程 OOPIF 子帧也能各自物化 utility world、跑 aria 注入。
///
/// **裁决⑥：只收 `type=="iframe"`，绝不收 `page`**（type 分流经 [`crate::tabs::should_arm_as_oopif`]）。
/// 本循环订阅的是**全局** attach 事件；若放行 `type=="page"`，看到**兄弟顶层 tab**（另一 page，
/// sid≠自己）会把它 arm 进自己的 `oopif_managers`，致 observe 活动 tab 时把兄弟整页内容当 OOPIF 子帧
/// 拼进来（**跨标签污染**）。顶层 page 归 tab 发现循环（[`spawn_tab_discovery_loop`] /
/// [`crate::tabs::should_arm_as_page`]）；二者各自筛 type，**严格互补、互不重叠**。
///
/// **现实（TODO(verify-oopif)）**：真 OOPIF 需跨源 http origin 才另起 `type=="iframe"` 子 session；
/// `file://` srcdoc/同源 iframe 是**同进程**（不另起子 session），故离线 fixture 触发不了这条路径。
/// 本循环是架构接线，真跨源路由须 http fixture / 真页后续验。所有错误 best-effort：单个子 session arm
/// 失败只 warn 不影响其它，绝不 panic。
fn spawn_oopif_arm_loop(
    conn: Connection,
    page_session: String,
    oopif_managers: std::sync::Arc<AsyncMutex<HashMap<String, OopifEntry>>>,
) -> tokio::task::JoinHandle<()> {
    use chromiumoxide::cdp::browser_protocol::target::EventAttachedToTarget;
    let mut rx = conn.subscribe(EventAttachedToTarget::IDENTIFIER, None);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let Ok(att) =
                        serde_json::from_value::<EventAttachedToTarget>(ev.params.clone())
                    else {
                        continue;
                    };
                    let sid: String = att.session_id.clone().into();
                    let ttype = att.target_info.r#type.clone();
                    // 裁决⑥：**只 arm `type=="iframe"` 的子 session**（跨进程 OOPIF 子帧）；跳过本 page
                    // session（已单独 arm）+ **`page`（兄弟顶层 tab，否则跨标签污染）** + service_worker/
                    // 其它。已 arm 过则跳过（CDP 可能对同 target 多次 attach）。type 分流经纯逻辑 helper
                    // [`crate::tabs::should_arm_as_oopif`]（与 should_arm_as_page 严格互补）。
                    let is_own_page_session = sid == page_session;
                    let already_armed = oopif_managers.lock().await.contains_key(&sid);
                    if !crate::tabs::should_arm_as_oopif(&ttype, is_own_page_session, already_armed) {
                        continue;
                    }
                    // 等子 session 在注册表登记（run_attach_loop 的 handle_attached 负责登记）。
                    let deadline =
                        tokio::time::Instant::now() + OOPIF_SESSION_REGISTER_TIMEOUT;
                    let registry = conn.registry().clone();
                    while !registry.has_session(&sid) {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    if !registry.has_session(&sid) {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            session_id = %sid,
                            "OOPIF child session never registered; skip arm (TODO(verify-oopif))"
                        );
                        continue;
                    }
                    // arm 该子 session 的注入管线。失败 best-effort：warn 后继续。
                    let manager = InjectionManager::new(conn.clone(), sid.clone());
                    match manager.arm().await {
                        Ok(loop_handle) => {
                            oopif_managers.lock().await.insert(
                                sid.clone(),
                                OopifEntry {
                                    manager,
                                    _loop: loop_handle,
                                },
                            );
                            tracing::debug!(
                                target: "nomi_browser_engine::backend::cdp",
                                session_id = %sid, target_type = %ttype,
                                "armed OOPIF child session injection (TODO(verify-oopif))"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "nomi_browser_engine::backend::cdp",
                                session_id = %sid, error = %e,
                                "arm OOPIF child session failed (non-fatal)"
                            );
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// **调试捕获后台循环**：启用 `Runtime.enable` + `Log.enable`（`Network.enable` 由 navigate 幂等
/// 调用，这里也 enable 确保非导航期网络事件亦被捕获），然后订阅事件流（长驻）写入 per-tab
/// [`crate::debug_capture::DebugBuffers`]。循环随连接关闭或 `.abort()` 终止。
///
/// 事件处理纯被动观察（**无** `Fetch.enable`/`requestPaused` 拦截——出口防火墙通道不碰）。
#[allow(clippy::collapsible_if)]
fn spawn_debug_capture_loop(
    conn: Connection,
    session_id: String,
    buffers: std::sync::Arc<std::sync::Mutex<crate::debug_capture::DebugBuffers>>,
) -> tokio::task::JoinHandle<()> {
    use chromiumoxide::cdp::js_protocol::runtime::EnableParams as RuntimeEnableParams;
    use chromiumoxide::cdp::browser_protocol::log::EnableParams as LogEnableParams;

    tokio::spawn(async move {
        // enable Runtime + Log（best-effort：失败仅 warn，不阻断整个 tab）。
        // Network.enable 由 navigate 路径已幂等调用；此处也 enable 兜底。
        let _ = conn
            .send::<RuntimeEnableParams>(&session_id, &RuntimeEnableParams::default())
            .await;
        let _ = conn
            .send::<LogEnableParams>(&session_id, &LogEnableParams::default())
            .await;
        let _ = conn
            .send::<NetworkEnableParams>(&session_id, &NetworkEnableParams::default())
            .await;

        // 订阅全部相关事件。
        let mut console_rx = conn.subscribe("Runtime.consoleAPICalled", Some(&session_id));
        let mut exception_rx = conn.subscribe("Runtime.exceptionThrown", Some(&session_id));
        let mut log_rx = conn.subscribe("Log.entryAdded", Some(&session_id));
        let mut req_rx = conn.subscribe("Network.requestWillBeSent", Some(&session_id));
        let mut resp_rx = conn.subscribe("Network.responseReceived", Some(&session_id));
        let mut fin_rx = conn.subscribe("Network.loadingFinished", Some(&session_id));
        let mut fail_rx = conn.subscribe("Network.loadingFailed", Some(&session_id));

        // 内部 requestId → 时间戳映射（用于算 duration_ms），有界。
        let mut request_timestamps: HashMap<String, f64> = HashMap::new();
        const TS_MAP_CAP: usize = 2000;

        loop {
            tokio::select! {
                biased;
                ev = console_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Some(entry) = crate::debug_capture::map_console_event(&e.params) {
                                if let Ok(mut b) = buffers.lock() {
                                    b.console.push(entry);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = exception_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Some(entry) = crate::debug_capture::map_exception_event(&e.params) {
                                if let Ok(mut b) = buffers.lock() {
                                    b.errors.push(entry);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = log_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Some(entry) = crate::debug_capture::map_log_error_event(&e.params) {
                                if let Ok(mut b) = buffers.lock() {
                                    b.errors.push(entry);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = req_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Some((id, entry)) = crate::debug_capture::map_request_will_be_sent(&e.params) {
                                // 记录 requestId 时间戳用于后续算 duration。
                                let ts = e.params.get("timestamp").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                if request_timestamps.len() >= TS_MAP_CAP {
                                    // 粗暴清理：超出上限清空（防无界增长，网络请求可能非常多）。
                                    request_timestamps.clear();
                                }
                                request_timestamps.insert(id.clone(), ts);
                                if let Ok(mut b) = buffers.lock() {
                                    b.network.push(entry);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = resp_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Some(_request_id) = e.params.get("requestId").and_then(|v| v.as_str()) {
                                if let Ok(mut b) = buffers.lock() {
                                    // 找到对应的 NetworkEntry 补全 response 信息（遍历 ring 尾部）。
                                    for entry in b.network.iter_mut() {
                                        if entry.url.is_empty() { continue; }
                                        // 按 URL+method 找不太可靠，用最近匹配（ring 尾 = 最新）。
                                        // 改进：在 entry 上存 request_id。暂且 patch 最后一个匹配的 pending entry。
                                    }
                                    // 简化实现：patch 最后一个 status==None 的 entry（近似匹配）。
                                    if let Some(last_pending) = b.network.iter_mut().rev().find(|e| e.status.is_none() && !e.failed) {
                                        crate::debug_capture::patch_response_received(last_pending, &e.params);
                                    }
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = fin_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            let req_id = e.params.get("requestId").and_then(|v| v.as_str()).unwrap_or("");
                            let req_ts = request_timestamps.get(req_id).copied().unwrap_or(0.0);
                            if let Ok(mut b) = buffers.lock() {
                                if let Some(entry) = b.network.iter_mut().rev().find(|e| e.duration_ms.is_none() && !e.failed && e.status.is_some()) {
                                    crate::debug_capture::patch_loading_finished(entry, &e.params, req_ts);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                ev = fail_rx.recv() => {
                    match ev {
                        Ok(e) => {
                            if let Ok(mut b) = buffers.lock() {
                                if let Some(entry) = b.network.iter_mut().rev().find(|e| !e.failed && e.status.is_none()) {
                                    crate::debug_capture::patch_loading_failed(entry, &e.params);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    })
}

#[async_trait]
impl BrowserEngine for CdpBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            browser_ready: true,
            headful: self.headful,
            display_available: self.display_available,
            engine: "chromium".into(),
        }
    }

    async fn navigate(&self, url: &str, _new_tab: bool) -> Result<NavResult, BrowserError> {
        // observe⊥act：整段导航持引擎 op_mutex（DESIGN §22），不与在途 observe/act 在本引擎交错。
        let _op = self.op_mutex.lock().await;
        // D1/D2：在 active tab 的 page session 上原地导航（多 tab 路由 / new_tab 由 D3 实现）。短暂
        // 锁 tabs 克隆出 active 句柄后立即释放（不跨 await 持 tabs 锁）。
        let handles = self.active_tab_handles().await?;
        let session = handles.session_id.as_str();
        let main_frame_id = handles.main_frame_id.clone();
        self.navigate_on_session(session, &main_frame_id, url).await
    }

    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError> {
        // observe⊥act：持 op_mutex（DESIGN §22），截图不与在途 act 改 DOM 交错。
        let _op = self.op_mutex.lock().await;
        // D1：截 active tab。
        let session = self.active_tab_handles().await?.session_id;
        let params = CaptureScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .build();

        let result = self
            .conn
            .send::<CaptureScreenshotParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;

        let shot: CaptureScreenshotReturns = serde_json::from_value(result.clone()).map_err(|e| {
            BrowserError::Other(format!("parse captureScreenshot response: {e}"))
        })?;
        // `data` 是 base64。用 chromiumoxide_types::Binary 的 AsRef<str> 取串后 decode。
        let b64: &str = shot.data.as_ref();
        decode_base64(b64).ok_or_else(|| {
            BrowserError::Other("captureScreenshot returned non-base64 data".into())
        })
    }

    async fn observe(&self, opts: &ObserveOpts) -> Result<Observation, BrowserError> {
        // observe⊥act：整段快照持 op_mutex，动作不可在序列化中途改 DOM（否则交回模型陈旧 ref）。
        let _op = self.op_mutex.lock().await;
        self.observe_impl(opts).await
    }

    async fn rendered_html(&self) -> Result<String, BrowserError> {
        // NOTE: 故意**不**持 op_mutex——只读 DOM 序列化（知识库管线消费），不得阻塞在途 act；其调用链
        // （active_frame_eval→active_page_frame→conn.send）不触碰被包裹的四个 trait 方法,无重入风险。
        // Read the **post-JS** DOM as raw HTML on the active frame. Read-only
        // (no redaction / no `<data>` wrap — see the trait doc): the knowledge
        // layer runs this through its own HTML→markdown pipeline, so it must get
        // un-transformed markup. `documentElement.outerHTML` is a plain string
        // returned by-value; empty document → empty string (best-effort guard so a
        // page with no documentElement never throws). `active_frame_eval` maps a JS
        // exception / transport failure into a `BrowserError` we surface as-is.
        let expression =
            "(() => { try { return document.documentElement ? document.documentElement.outerHTML : ''; } catch (e) { return ''; } })()";
        let value = self.active_frame_eval(expression).await?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    async fn act(
        &self,
        spec: &crate::actions::ActSpec,
        progress: &crate::progress::Progress,
    ) -> Result<crate::actions::ActResult, BrowserError> {
        // observe⊥act + per-target act 串行：一引擎一次一动作。受 progress 截止时间界定,op_mutex 不无界持有。
        let _op = self.op_mutex.lock().await;
        // C1：Click/Type/SetValue 经 act_impl 串 B2-B6 执行；其它 ActSpec 仍 Unsupported（C2/C3/D/E/F）。
        self.act_impl(spec, progress).await
    }

    async fn debug_snapshot(
        &self,
    ) -> Result<crate::debug_capture::DebugSnapshot, BrowserError> {
        let handles = self.active_tab_handles().await?;
        Ok(crate::debug_capture::DebugSnapshot::from_buffers(&handles.debug))
    }

    async fn bring_to_front(&self) -> Result<(), BrowserError> {
        CdpBackend::bring_to_front(self).await
    }

    async fn capture_storage_state(
        &self,
    ) -> Result<crate::storage_state::StorageState, BrowserError> {
        CdpBackend::capture_storage_state(self).await
    }

    async fn click_at_css_point(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        use crate::input::Point;
        self.click_at(Point { x, y }).await
    }

    async fn device_pixel_ratio(&self) -> Result<f64, BrowserError> {
        // Query the active page's window.devicePixelRatio (P7B: visual-fallback coord mapping).
        // Best-effort: any exception / non-positive / non-number → fall back to 1.0 (never block
        // a visual-fallback click on a DPR probe; 1.0 is correct for headless anyway).
        let session = self.active_tab_handles().await?.session_id;
        let mut params = EvaluateParams::new("window.devicePixelRatio".to_string());
        params.return_by_value = Some(true);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        let dpr = result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(serde_json::Value::as_f64)
            .filter(|d| *d > 0.0)
            .unwrap_or(1.0);
        Ok(dpr)
    }
}

impl CdpBackend {
    /// **detach/crash 事件源 → `Progress::abort` 接线**（B6，DESIGN §11/§22）：在一次 `act` 期间
    /// **临时订阅** `Target.detachedFromTarget` / `Target.targetCrashed`（page target 没了/崩了）
    /// 与 `Page.frameDetached`（动作所在帧从树上 detach），把它们映射到对当前动作 [`Progress`] 的
    /// `abort(PageClosed|FrameDetached)`，使进行中的动作（在 [`crate::actions::run_act_with_retry`]
    /// 的 `progress.race` 上）**立即取消**（远早于 deadline），而非白等超时。
    ///
    /// 形态（**最小可行**，C1 复用）：
    /// 1. 据传入的 `parent`（动作的总 deadline/取消上下文）派生一个**子** [`Progress`]（共享 timeout +
    ///    token 层级：parent 取消 → 子立即取消）。动作跑在返回的子 Progress 上。
    /// 2. spawn 一个监听任务，select 三路事件订阅；命中即对子 Progress `abort`：
    ///    - `Target.detachedFromTarget` / `Target.targetCrashed`（params.sessionId == 本 page session）
    ///      → `abort(PageClosed)`（target 没了/崩了 → 上层经 errmap 成 `TargetClosed`）；
    ///    - `Page.frameDetached`（params.frameId == `frame_id`，即动作所在帧）→ `abort(FrameDetached)`。
    /// 3. 返回 `(child, guard)`：动作在 `child` 上跑；`guard` 持监听任务句柄，**Drop 即取消监听**
    ///    （动作结束——成功/失败均然——guard 离开作用域，临时订阅随之收摊）。
    ///
    /// **绝不 panic**：监听任务里所有解析失败 best-effort（continue）；订阅通道关闭/落后按 broadcast
    /// 语义处理（closed→退出、lagged→继续）。crash 与 close 的恢复语义一致（重开 target），故都映射到
    /// `PageClosed`（`AbortReason` 不含独立 crash 变体；这是本任务约定的最小接线，dedicated crash
    /// reason 留待需要时扩 progress.rs/errmap.rs）。
    ///
    /// **D1：async**——内部短暂锁 tabs 取 active tab 的 page session（订阅 Page.frameDetached 限定它 +
    /// 比对 detach/crash 事件的 sessionId）后立即释放。active tab 缺失 → Err（绝不 panic）。
    pub async fn arm_act_abort(
        &self,
        parent: &Progress,
        frame_id: &str,
    ) -> Result<(Arc<Progress>, ActAbortGuard), BrowserError> {
        // D1：取 active tab 的 page session（detach/crash 订阅 + sessionId 比对锚定它）。
        let page_session = self.active_tab_handles().await?.session_id;

        // 子 Progress：共享 parent 的剩余 deadline（保守用 parent 当下剩余预算的近似——这里直接复用
        // parent 的 token 层级，timeout 取一个不短于 parent 的值；act 的真实 deadline 由调用方在 parent
        // 上设定，子继承其取消）。用 child(timeout, parent_token) 建层级：parent 取消 → 子立即取消。
        let child = Arc::new(Progress::child(parent.timeout(), parent.token()));

        // 三路临时订阅（act 期间有效，guard drop 后监听任务被 abort，订阅 Receiver 随任务 drop 收摊）。
        let mut detached_rx = self.conn.subscribe("Target.detachedFromTarget", None);
        let mut crashed_rx = self.conn.subscribe("Target.targetCrashed", None);
        let mut frame_detached_rx = self
            .conn
            .subscribe("Page.frameDetached", Some(&page_session));

        let watch_frame = frame_id.to_string();
        let child_for_task = Arc::clone(&child);

        let handle = tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                tokio::select! {
                    // page target detach（tab 关闭）：params.sessionId == 本 page session → PageClosed。
                    ev = detached_rx.recv() => match ev {
                        Ok(ev) => {
                            if event_session_matches(&ev.params, &page_session) {
                                child_for_task.abort(AbortReason::PageClosed);
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                    // page target crash：同样按 sessionId 命中 → PageClosed（恢复语义同 close）。
                    ev = crashed_rx.recv() => match ev {
                        Ok(ev) => {
                            if event_session_matches(&ev.params, &page_session) {
                                child_for_task.abort(AbortReason::PageClosed);
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                    // 动作所在帧 detach：params.frameId == watch_frame → FrameDetached。
                    ev = frame_detached_rx.recv() => match ev {
                        Ok(ev) => {
                            if event_frame_matches(&ev.params, &watch_frame) {
                                child_for_task.abort(AbortReason::FrameDetached);
                                break;
                            }
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    },
                }
            }
        });

        Ok((child, ActAbortGuard { handle: Some(handle) }))
    }

    /// 查当前 page 的 url：`Page.getNavigationHistory` 取 currentIndex 指向的 entry url。
    /// 失败/取不到返回 None（调用方回退）。
    async fn current_url(&self, session: &str) -> Option<String> {
        use chromiumoxide::cdp::browser_protocol::page::GetNavigationHistoryParams;
        let result = self
            .conn
            .send::<GetNavigationHistoryParams>(session, &GetNavigationHistoryParams::default())
            .await
            .ok()?;
        let idx = result.get("currentIndex")?.as_i64()?;
        let entries = result.get("entries")?.as_array()?;
        let entry = entries.get(usize::try_from(idx).ok()?)?;
        entry.get("url")?.as_str().map(|s| s.to_string())
    }

    /// SD-4：一次 `Page.getNavigationHistory` 同时取 url + POST 标志（`observe_impl` 用）。
    /// 失败 → `(None, false)`（保守：不误判普通页 reload 为不可逆）。
    async fn url_and_post_flag(&self, session: &str) -> (Option<String>, bool) {
        use chromiumoxide::cdp::browser_protocol::page::GetNavigationHistoryParams;
        let result = match self
            .conn
            .send::<GetNavigationHistoryParams>(session, &GetNavigationHistoryParams::default())
            .await
        {
            Ok(v) => v,
            Err(_) => return (None, false),
        };
        let current_index = match result.get("currentIndex").and_then(|v| v.as_i64()) {
            Some(idx) => idx,
            None => return (None, false),
        };
        let entries_val = match result.get("entries") {
            Some(v) => v.clone(),
            None => return (None, false),
        };
        let url = entries_val
            .as_array()
            .and_then(|arr| usize::try_from(current_index).ok().and_then(|i| arr.get(i)))
            .and_then(|e| e.get("url"))
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let is_post = nav::current_entry_is_post(&entries_val, current_index);
        (url, is_post)
    }

    /// **navigate settle 全链**（D2，DESIGN §12 + 裁决⑤）：在给定 page session 上原地导航，跑成熟
    /// 的生命周期判定后返回 [`NavResult`]。D3 的 new_tab/switch 走别的 session，本方法只管「一个
    /// session 上的一次导航」。
    ///
    /// 时序（先订阅后导航，避免快页面在订阅前就 load 完）：
    /// 1. `Page.enable` + `Network.enable`（生命周期 + 网络事件前置；幂等）。
    /// 2. **先订阅** 四路事件（全在本 session 上）：`domContentEventFired` / `loadEventFired` /
    ///    `navigatedWithinDocument`（SPA）/ `requestWillBeSent` / `responseReceived` /
    ///    `loadingFinished` / `loadingFailed`。
    /// 3. `Page.navigate`（errorText 即导航失败 → `NavFailed`）。
    /// 4. **settle 阶梯**（[`Self::run_settle`]）：等 DOMContentLoaded → 短 settle → 升级 Load；
    ///    其间 SPA 软导航信号 → 走软导航降级路径（不重新等 load）。
    /// 5. **networkidle 独立短 cap**（[`Self::wait_network_idle`]）：仅在已达 Load 后做；inflight 持续
    ///    0 满 500ms → `NetworkIdle`；到 4s cap 仍未达成（长轮询站）→ 降级回 `Load`。**绝不并入**
    ///    导航总超时。
    /// 6. http_status 从主帧 Document `responseReceived` 取；final_url 查 history；redirected 用
    ///    [`nav::is_redirect`] 归一化比较（**非裸 `!=`**）。
    ///
    /// 良性态不报错：networkidle cap 降级 / SPA 软导航 / 302 重定向都 `Ok`（success 语义在 facade）。
    async fn navigate_on_session(
        &self,
        session: &str,
        main_frame_id: &str,
        url: &str,
    ) -> Result<NavResult, BrowserError> {
        // navigate 的触发 = `Page.navigate`（errorText 即导航失败 → NavFailed）。settle 全链复用
        // [`Self::settle_after_trigger`]（D2 母本；back/forward/reload 同样复用它，零分叉）。
        self.settle_after_trigger(session, main_frame_id, url, |conn, session| async move {
            let nav_params = NavigateParams::new(url);
            let nav_result = conn
                .send::<NavigateParams>(&session, &nav_params)
                .await
                .map_err(map_transport_err)?;
            let nav: NavigateReturns = serde_json::from_value(nav_result.clone()).map_err(|e| {
                BrowserError::Other(format!("parse Page.navigate response: {e} (raw={nav_result})"))
            })?;
            if let Some(err_text) = nav.error_text.as_deref() {
                return Err(BrowserError::NavFailed {
                    kind: err_text.to_string(),
                });
            }
            Ok(())
        })
        .await
    }

    /// **导航 settle 母本（D2 抽出，D4 复用）**：在 `session` 上**先订阅**全部生命周期/网络事件，
    /// 再跑 `trigger`（发那条引发导航的 CDP 命令——`Page.navigate` / `navigateToHistoryEntry` /
    /// `Page.reload`），随后跑成熟的 settle 阶梯（[`Self::run_settle`] + networkidle 短 cap）并返回
    /// [`NavResult`]。**back/forward/reload 与 navigate 共用本方法**——它们只在 `trigger` 不同
    /// （settle 逻辑零分叉，对齐 D4「settle 复用 D2 run_settle」）。
    ///
    /// 时序与 D2 一致（见 [`Self::navigate_on_session`] 旧 doc）：enable → 订阅 7 路事件 → trigger →
    /// run_settle（DCL→短 settle→Load；SPA 软导航降级）→ networkidle 独立短 cap → final_url +
    /// redirected（URL-normalize 比较）+ http_status（主帧 Document responseReceived）。
    ///
    /// `expected_url` 用于 redirect 归一化比较的「from」端：navigate 传请求 url；reload/history 导航传
    /// 触发**前**的当前 url（reload 通常不变 → 不算 redirect；history 导航回到的 entry url 即「目标」，
    /// 与 final_url 同源 → 不算 redirect）。良性态（cap 降级 / SPA / 302）皆 `Ok`。
    async fn settle_after_trigger<F, Fut>(
        &self,
        session: &str,
        main_frame_id: &str,
        expected_url: &str,
        trigger: F,
    ) -> Result<NavResult, BrowserError>
    where
        F: FnOnce(Connection, String) -> Fut,
        Fut: std::future::Future<Output = Result<(), BrowserError>>,
    {
        // 1) Page.enable + Network.enable（生命周期 + 网络事件前置；幂等）。
        self.conn
            .send::<PageEnableParams>(session, &PageEnableParams::default())
            .await
            .map_err(map_transport_err)?;
        self.conn
            .send::<NetworkEnableParams>(session, &NetworkEnableParams::default())
            .await
            .map_err(map_transport_err)?;

        // 2) 先订阅全部相关事件（trigger 之前，避免漏掉早到的 DCL/load/response）。
        let mut dcl_rx = self.conn.subscribe("Page.domContentEventFired", Some(session));
        let mut load_rx = self.conn.subscribe("Page.loadEventFired", Some(session));
        let mut spa_rx = self
            .conn
            .subscribe("Page.navigatedWithinDocument", Some(session));
        let mut response_rx = self.conn.subscribe("Network.responseReceived", Some(session));
        let mut req_rx = self.conn.subscribe("Network.requestWillBeSent", Some(session));
        let mut fin_rx = self.conn.subscribe("Network.loadingFinished", Some(session));
        let mut fail_rx = self.conn.subscribe("Network.loadingFailed", Some(session));

        let mut http_status: Option<u16> = None;
        let mut inflight = InflightCounter::new();

        // 3) 触发导航（navigate / navigateToHistoryEntry / reload）。失败 → 上抛（NavFailed/传输错）。
        trigger(self.conn.clone(), session.to_string()).await?;

        // 4) settle 阶梯（DCL → 短 settle → Load；SPA 软导航降级；记 http_status + inflight）。
        let settle = self
            .run_settle(
                main_frame_id,
                &mut dcl_rx,
                &mut load_rx,
                &mut spa_rx,
                &mut response_rx,
                &mut req_rx,
                &mut fin_rx,
                &mut fail_rx,
                &mut http_status,
                &mut inflight,
            )
            .await;

        // 5) 决定 load_state（SPA 软导航 / Load 后 networkidle 短 cap / 降级——与 D2 一致）。
        let base_state = match settle.state {
            NavSettleState::Load => LoadState::Load,
            NavSettleState::DomContentLoaded => LoadState::DomContentLoaded,
            NavSettleState::Commit => LoadState::Commit,
        };
        let load_state = if settle.soft_nav {
            base_state
        } else if base_state == LoadState::Load {
            self.wait_network_idle(&mut inflight, &mut req_rx, &mut fin_rx, &mut fail_rx)
                .await
        } else {
            base_state
        };

        // 6) final_url + redirected（归一化比较，非裸 !=）+ http_status。
        let final_url = self
            .current_url(session)
            .await
            .unwrap_or_else(|| expected_url.to_string());
        let redirected = nav::is_redirect(expected_url, &final_url);

        Ok(NavResult {
            final_url,
            http_status,
            redirected,
            load_state,
        })
    }

    /// **settle 阶梯执行**（[`Self::navigate_on_session`] step 4）：等 DOMContentLoaded → 短 settle →
    /// Load，其间持续吸收主帧 Document responseReceived（填 http_status）+ inflight 事件（喂计数器）。
    ///
    /// 各阶段都有自己的短上限（不依赖单一大超时；总预算另由传输层每命令 30s + 这些上限兜底）：
    /// - 等 DCL：[`nav::DOMCONTENTLOADED_TIMEOUT`]（30s 上限，超时不致命 → 停在 Commit）。
    /// - 收到 DCL 后短 settle：[`nav::SETTLE_QUIET`]（100ms，给同步脚本/首批微任务喘息）。
    /// - 短 settle 后等 Load：剩余预算（载入子资源；超时不致命 → 停在 DomContentLoaded）。
    ///
    /// **SPA 软导航**：任何阶段收到 `navigatedWithinDocument` → 走 [`Self::wait_spa_soft_nav`]
    /// 降级（等 URL 变 / 短稳定），置 `soft_nav=true` 立即返回（不重新等 load）。
    #[allow(clippy::too_many_arguments)]
    async fn run_settle(
        &self,
        main_frame_id: &str,
        dcl_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        load_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        spa_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        response_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        req_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        fin_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        fail_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        http_status: &mut Option<u16>,
        inflight: &mut InflightCounter,
    ) -> SettleOutcome {
        let mut state = NavSettleState::Commit;

        // ── 阶段 1：等 DOMContentLoaded（其间吸收 response/inflight/SPA 信号）──
        let dcl_deadline = tokio::time::Instant::now() + nav::DOMCONTENTLOADED_TIMEOUT;
        loop {
            let remaining = dcl_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // DCL 超时：停在 Commit（文档已提交但 DOM 没等到——慢站；良性，不报错）。
                return SettleOutcome { state, soft_nav: false };
            }
            tokio::select! {
                biased;
                // SPA 软导航优先：收到即降级路径，不再等 load。
                ev = spa_rx.recv() => {
                    if recv_ok(ev) {
                        self.wait_spa_soft_nav().await;
                        return SettleOutcome { state, soft_nav: true };
                    }
                }
                ev = dcl_rx.recv() => {
                    if recv_ok(ev) {
                        state = nav::advance_settle(state, LifecycleSignal::DomContentLoaded);
                        break;
                    }
                }
                // load 可能先于 DCL 订阅被处理到（极快页面）→ 直接拔高到 Load 并跳出。
                ev = load_rx.recv() => {
                    if recv_ok(ev) {
                        state = nav::advance_settle(state, LifecycleSignal::Load);
                        // 已 Load 必已过 DCL；直接进短 settle 后返回。
                        tokio::time::sleep(nav::SETTLE_QUIET).await;
                        return SettleOutcome { state, soft_nav: false };
                    }
                }
                ev = response_rx.recv() => { absorb_response(ev, main_frame_id, http_status); }
                ev = req_rx.recv() => { absorb_request(ev, inflight); }
                ev = fin_rx.recv() => { absorb_finish(ev, inflight); }
                ev = fail_rx.recv() => { absorb_fail(ev, inflight); }
                () = tokio::time::sleep(remaining) => {
                    return SettleOutcome { state, soft_nav: false };
                }
            }
        }

        // ── 阶段 2：短 settle（给同步脚本/首批微任务喘息；其间仍吸收事件）──
        let settle_deadline = tokio::time::Instant::now() + SETTLE_QUIET;
        loop {
            let remaining = settle_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                biased;
                ev = spa_rx.recv() => {
                    if recv_ok(ev) {
                        self.wait_spa_soft_nav().await;
                        return SettleOutcome { state, soft_nav: true };
                    }
                }
                ev = load_rx.recv() => {
                    if recv_ok(ev) {
                        state = nav::advance_settle(state, LifecycleSignal::Load);
                        return SettleOutcome { state, soft_nav: false };
                    }
                }
                ev = response_rx.recv() => { absorb_response(ev, main_frame_id, http_status); }
                ev = req_rx.recv() => { absorb_request(ev, inflight); }
                ev = fin_rx.recv() => { absorb_finish(ev, inflight); }
                ev = fail_rx.recv() => { absorb_fail(ev, inflight); }
                () = tokio::time::sleep(remaining) => { break; }
            }
        }

        // ── 阶段 3：等 Load（载入子资源；超时不致命 → 停在 DomContentLoaded）──
        // 可交互探测：到此 DOM 已构建（DCL 过）+ 短 settle 已过——「可交互元素出现」对应 DOM 就绪，
        // observe 链会在调用方真正反查元素时把关；这里用「等 load / 短上限内」作为「可交互稳态」近似。
        let load_deadline = tokio::time::Instant::now() + nav::DOMCONTENTLOADED_TIMEOUT;
        loop {
            let remaining = load_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                biased;
                ev = spa_rx.recv() => {
                    if recv_ok(ev) {
                        self.wait_spa_soft_nav().await;
                        return SettleOutcome { state, soft_nav: true };
                    }
                }
                ev = load_rx.recv() => {
                    if recv_ok(ev) {
                        state = nav::advance_settle(state, LifecycleSignal::Load);
                        break;
                    }
                }
                ev = response_rx.recv() => { absorb_response(ev, main_frame_id, http_status); }
                ev = req_rx.recv() => { absorb_request(ev, inflight); }
                ev = fin_rx.recv() => { absorb_finish(ev, inflight); }
                ev = fail_rx.recv() => { absorb_fail(ev, inflight); }
                () = tokio::time::sleep(remaining) => { break; }
            }
        }

        SettleOutcome { state, soft_nav: false }
    }

    /// **SPA 软导航降级**（DESIGN §12：same-document，无 newDocument → 不重新等 load）：等一个短稳定
    /// 窗口让软导航落地（URL 已由 navigatedWithinDocument 改了 history）。这里用 [`SPA_SETTLE_TIMEOUT`]
    /// 内的固定短 sleep 作为「下一目标 actionable 前的稳定点」——真正的 actionable 由调用方下一步
    /// observe/act 的 actionability 把关，本方法只兑现「软导航后不白等 load 超时」。
    async fn wait_spa_soft_nav(&self) {
        // 软导航通常瞬时；给一个远小于 SPA_SETTLE_TIMEOUT 的稳定窗口即可。封顶 SPA_SETTLE_TIMEOUT。
        let quiet = SETTLE_QUIET.min(SPA_SETTLE_TIMEOUT);
        tokio::time::sleep(quiet).await;
    }

    /// **networkidle 独立短 cap 等待**（DESIGN §12 + 裁决⑤）：在已达 Load 后调用。持续观察 inflight
    /// 事件，inflight 连续为 0 满 [`NETWORK_IDLE_QUIET`]（500ms）→ 返 `NetworkIdle`；到
    /// [`NETWORK_IDLE_CAP`]（4s）仍未达成（长轮询/SSE/WS 永不 idle）→ **降级返 `Load`**。
    ///
    /// **关键不变量**：这个 cap **完全独立**于导航总超时——4s 内拿不到 networkidle 就退而求其次返
    /// Load，绝不让长轮询站把整个 navigate 拖到 30s。良性态（cap 降级）不报错。
    async fn wait_network_idle(
        &self,
        inflight: &mut InflightCounter,
        req_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        fin_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
        fail_rx: &mut tokio::sync::broadcast::Receiver<crate::transport::CdpEvent>,
    ) -> LoadState {
        let cap_deadline = tokio::time::Instant::now() + NETWORK_IDLE_CAP;
        loop {
            // 当前已空闲 → 进入「连续空闲 500ms」计时；其间任何新请求 → 重新计时（回外层 loop）。
            if inflight.is_idle() {
                let quiet_until = tokio::time::Instant::now() + NETWORK_IDLE_QUIET;
                loop {
                    let now = tokio::time::Instant::now();
                    // 整段 cap 已到 → 即便正接近 quiet 也以 cap 为硬上限降级。
                    let cap_left = cap_deadline.saturating_duration_since(now);
                    let quiet_left = quiet_until.saturating_duration_since(now);
                    if quiet_left.is_zero() {
                        // 连续空闲满 500ms → networkidle 达成。
                        return LoadState::NetworkIdle;
                    }
                    if cap_left.is_zero() {
                        // cap 到（长轮询站永不 idle）→ 降级 Load（良性，不报错）。
                        return LoadState::Load;
                    }
                    // 等到 quiet 满 或 有新请求打破空闲（取较小窗口，cap 兜底）。
                    let wait = quiet_left.min(cap_left);
                    tokio::select! {
                        biased;
                        ev = req_rx.recv() => {
                            absorb_request(ev, inflight);
                            if !inflight.is_idle() { break; } // 空闲被打破 → 回外层重新等空闲
                        }
                        ev = fin_rx.recv() => { absorb_finish(ev, inflight); }
                        ev = fail_rx.recv() => { absorb_fail(ev, inflight); }
                        () = tokio::time::sleep(wait) => { /* 重新评估 quiet/cap */ }
                    }
                }
            } else {
                // 非空闲：等到空闲 或 cap 到。
                let cap_left = cap_deadline.saturating_duration_since(tokio::time::Instant::now());
                if cap_left.is_zero() {
                    return LoadState::Load;
                }
                tokio::select! {
                    biased;
                    ev = req_rx.recv() => { absorb_request(ev, inflight); }
                    ev = fin_rx.recv() => { absorb_finish(ev, inflight); }
                    ev = fail_rx.recv() => { absorb_fail(ev, inflight); }
                    () = tokio::time::sleep(cap_left) => { return LoadState::Load; }
                }
            }
        }
    }


    /// **observe 全链**（Task 6）：逐帧 `incrementalAriaSnapshot` → 缝合 → 脱敏 → 代际翻新 ref 表。
    ///
    /// **D1**：开头一次 [`Self::active_tab_handles`] 拿 active tab 句柄快照（session/injection/
    /// main_frame/oopif/ref_table），全链用它——不跨 await 持 `tabs` 锁（ref_table 是克隆出的 Arc，
    /// 锁外独立锁）。
    ///
    /// 步骤：
    /// 1. 等主帧 utility context 就绪（导航后短轮询；超时即报 NavFailed{context}）。
    /// 2. `DOM.enable`（getFrameOwner/resolveNode 的前置）。
    /// 3. 列同进程帧（frameTree）+ 续编 OOPIF 子 session，逐帧产 [`FrameSnapshot`]（seq → prefix `f<seq>`）。
    /// 4. 建 `child_frame → (parent_frame, parent_iframe_ref)` 路由（getFrameOwner→resolveNode→`_ariaRef.ref`）。
    /// 5. 自主帧起递归 [`stitch`] 成一棵树。
    /// 6. 脱敏（[`redact::redact_yaml`]）+ 不可信包裹（[`redact::wrap_untrusted`]，origin=current_url）。
    /// 7. 代际翻新：锁 active tab 的 `ref_table`，`new_generation(prev)`，解析每行 `[ref=...]` 填 entries + 表，存回。
    ///
    /// 所有 CDP/注入调用经 `map_transport_err`/`map_inject_err`，**绝不 panic**。
    pub(crate) async fn observe_impl(&self, opts: &ObserveOpts) -> Result<Observation, BrowserError> {
        // D1：一次拿 active tab 句柄快照（立即释放 tabs 锁）。全链用 handles。
        let handles = self.active_tab_handles().await?;
        let page_session = handles.session_id.clone();

        // 1) 等主帧 utility context 就绪（fresh navigate 后 world 可能还没物化）。
        self.wait_main_context_ready(&handles).await?;

        // 2) DOM.enable（iframe→子帧路由前置）。幂等。
        self.conn
            .send::<DomEnableParams>(&page_session, &DomEnableParams::default())
            .await
            .map_err(map_transport_err)?;

        // 3) 逐帧产 FrameSnapshot。frames: (seq, frame_id, session_id, snapshot)。
        let mut frames: Vec<ObservedFrame> = Vec::new();
        let mut next_seq: u32 = 0;
        let mut truncated = false;
        // D5：跨帧累积 password 输入的 aria ref（同帧 utility world 收集），缝合后宿主侧抹其 value。
        let mut password_refs: Vec<String> = Vec::new();
        // D5 fail-closed：任一帧 password 探测失败的标志。失败时无法精确知道哪些字段是 password，
        // 故对全部可编辑控件值整体 over-redact 兜底（绝不放行 password 明文）。
        let mut any_password_query_failed = false;
        // P7B：主帧可点击 ref 的 CSS 像素框（仅当 opts.include_boxes）。**仅主帧**（方案①）：
        // getBoundingClientRect 是帧内视口坐标，主帧 viewport 即截图坐标系；子帧需叠 iframe 偏移（方案②暂缓）。
        let mut ref_boxes: std::collections::HashMap<String, crate::engine::CssRect> =
            std::collections::HashMap::new();

        // 3a) 同进程帧（active tab page session 的 frameTree；主帧在前）。
        let same_proc_frames = handles.injection.frame_ids().await.map_err(map_inject_err)?;
        for fid in &same_proc_frames {
            match self
                .snapshot_one_frame(&handles.injection, fid, next_seq, opts)
                .await
            {
                Ok(Some(snap)) => {
                    if frame_hit_depth_limit(&snap, opts.max_depth) {
                        truncated = true;
                    }
                    // D5：该帧 password 字段的 ref（同 utility world）。查询失败 → 置 fail-closed 标志。
                    if collect_password_refs(&handles.injection, fid, &mut password_refs).await {
                        any_password_query_failed = true;
                    }
                    // P7B：仅主帧采集可点击 ref 的 CSS 框（方案①）。紧接该帧 snapshot 之后取，确保
                    // 该帧 _lastAriaSnapshotForQuery 刚物化、未被后续帧覆盖。best-effort：失败仅 warn，
                    // 不影响 observe（拿不到框 → facade 不画 SoM、回落原始兜底）。
                    if opts.include_boxes && fid == &handles.main_frame_id {
                        match handles.injection.ref_boxes(fid).await {
                            Ok(b) => ref_boxes = b,
                            Err(e) => tracing::warn!(
                                target: "nomi_browser_engine::backend::cdp",
                                frame_id = %fid, error = ?e,
                                "ref_boxes (SoM geometry) failed for main frame (skip; visual fallback degrades to raw)"
                            ),
                        }
                    }
                    frames.push(ObservedFrame {
                        seq: next_seq,
                        frame_id: fid.clone(),
                        session_id: page_session.clone(),
                        snapshot: snap,
                    });
                    next_seq += 1;
                }
                // context 没就绪 / body 取不到 / 单帧 JS 异常：跳过该帧（best-effort，不致命）。
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "nomi_browser_engine::backend::cdp",
                        frame_id = %fid, error = ?e,
                        "snapshot same-process frame failed (skip)"
                    );
                }
            }
        }

        // 3b) OOPIF 子 session（跨进程子帧；接线骨架，离线 fixture 触发不到，见 TODO(verify-oopif)）。
        // 锁内**只** clone 出各 OOPIF manager 句柄（InjectionManager: Clone，共享 Arc 缓存）后立即
        // 释放锁；所有 `.await`（frame_ids / snapshot / password_refs）在锁外跑——兑现「不跨 await
        // 持锁」（避免阻塞 spawn_oopif_arm_loop 的插入）。manager 克隆不复制后台循环，但经共享 Arc
        // 读同一份 context 真相。D1：用 active tab 的 oopif_managers（克隆出的 Arc）。
        let oopif_managers: Vec<(String, InjectionManager)> = {
            let guard = handles.oopif_managers.lock().await;
            guard
                .iter()
                .map(|(sid, entry)| (sid.clone(), entry.manager.clone()))
                .collect()
        };
        for (oopif_session, manager) in oopif_managers {
            // TODO(verify-oopif): 跨源 OOPIF 须 http fixture / 真页验；此处架构接线。
            let Ok(fids) = manager.frame_ids().await else {
                continue;
            };
            for fid in &fids {
                match self.snapshot_one_frame(&manager, fid, next_seq, opts).await {
                    Ok(Some(snap)) => {
                        if frame_hit_depth_limit(&snap, opts.max_depth) {
                            truncated = true;
                        }
                        // D5：OOPIF 子帧 password ref（其自有 utility world）。失败 → fail-closed。
                        if collect_password_refs(&manager, fid, &mut password_refs).await {
                            any_password_query_failed = true;
                        }
                        frames.push(ObservedFrame {
                            seq: next_seq,
                            frame_id: fid.clone(),
                            session_id: oopif_session.clone(),
                            snapshot: snap,
                        });
                        next_seq += 1;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            frame_id = %fid, error = ?e,
                            "snapshot OOPIF frame failed (skip) (TODO(verify-oopif))"
                        );
                    }
                }
            }
        }

        // 没拍到任何帧（极端：context 始终没就绪）→ 报 context 未就绪而非空白。
        if frames.is_empty() {
            return Err(BrowserError::NavFailed {
                kind: "context".into(),
            });
        }

        // 4) 建 iframe→子帧 路由：child_frame_id → (parent_frame_id, parent_iframe_ref)。
        let parent_of = self.build_iframe_routing(&handles, &frames).await;

        // 5) 自主帧（seq=0 / frame_id == main_frame_id）起递归缝合。
        let main_idx = frames
            .iter()
            .position(|f| f.frame_id == handles.main_frame_id)
            .unwrap_or(0);
        let stitched = render_frame_recursive(&frames, main_idx, &parent_of);

        // 6) D5 password value 置空 → 脱敏 → 不可信包裹（origin = 当前 url）。
        //    正常路径：按 utility world 收集到的 password ref 精确抹掉内联 value（DOM type=password
        //    信号，不误伤普通 textbox）。**fail-closed**：若任一帧 password 探测失败，无法精确知道
        //    哪些是 password，额外对全部可编辑控件值整体 over-redact 兜底（绝不放行明文）。
        //    之后再跑正则/高熵脱敏，最后 <data> 包裹。
        //
        // SD-4：同一次 nav-history 查询同时取 url + POST 标志（避免额外 CDP round-trip）。
        let (url, current_page_is_post) = self.url_and_post_flag(&page_session).await;
        let blanked = redact::blank_secret_values(&stitched, &password_refs);
        let blanked = if any_password_query_failed {
            tracing::warn!(
                target: "nomi_browser_engine::backend::cdp",
                "password 探测失败，对可编辑控件值整体 over-redact 以防泄露 (fail-closed)"
            );
            redact::blank_all_editable_values(&blanked)
        } else {
            blanked
        };
        let redacted = redact::redact_yaml(&blanked);
        let yaml = redact::wrap_untrusted(&redacted, url.as_deref());

        // 7) 代际翻新 + entries/ref 表。注意：ref 表与 entries 用**脱敏前**的 stitched 解析
        //    （脱敏只动 secret 文本，不动 role/ref；但用 stitched 保证 ref 行完整不被 <data> 包裹干扰）。
        //    D1：锁 active tab 的 ref_table（克隆出的 Arc，per-tab 隔离）。
        let (generation, entries) = {
            let mut guard = handles.ref_table.lock().await;
            let mut table = RefTable::new_generation(guard.as_ref());
            let generation_id = table.generation();
            let entries = self.parse_refs_into_table(&frames, &stitched, &mut table);
            *guard = Some(table);
            (generation_id, entries)
        };

        Ok(Observation {
            generation,
            yaml,
            entries,
            url,
            truncated,
            current_page_is_post,
            boxes: ref_boxes,
        })
    }

    /// 短轮询等主帧的 utility-world context 就绪（fresh navigate 后 world 物化有延迟）。
    /// 超时 → `NavFailed{kind:"context"}`（语义：这次没拿到可用上下文，调用方可重试）。
    /// **D1**：用传入 active tab 句柄的注入管线 + 主帧 id（observe_impl 已 clone 出，不再读字段）。
    async fn wait_main_context_ready(&self, handles: &TabHandles) -> Result<(), BrowserError> {
        let deadline = tokio::time::Instant::now() + OBSERVE_CONTEXT_READY_TIMEOUT;
        loop {
            if handles
                .injection
                .context_id_for(&handles.main_frame_id)
                .is_ok()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserError::NavFailed {
                    kind: "context".into(),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 拍单帧：取 body objectId → `incrementalAriaSnapshot(body, {mode:ai, refPrefix:f<seq>, depth, track})`
    /// → 反序列化成 [`FrameSnapshot`]。context 未就绪 / body 取不到 → `Ok(None)`（best-effort 跳过）；
    /// JS 异常 / 协议错误 → `Err`（调用方 warn 后跳过单帧）。
    async fn snapshot_one_frame(
        &self,
        manager: &InjectionManager,
        frame_id: &str,
        seq: u32,
        opts: &ObserveOpts,
    ) -> Result<Option<FrameSnapshot>, InjectError> {
        // context 未就绪 / body null → 视为该帧暂不可观测，跳过（不报错）。
        let body_obj_id = match manager.body_object_id(frame_id).await {
            Ok(id) => id,
            Err(InjectError::ContextNotReady { .. }) => return Ok(None),
            // body 为 null（空文档）→ Protocol；当作不可观测帧跳过。
            Err(InjectError::Protocol(_)) => return Ok(None),
            Err(other) => return Err(other),
        };

        let prefix = frame_prefix(seq);
        let node_arg = CallArgument {
            object_id: Some(RemoteObjectId::new(body_obj_id)),
            ..Default::default()
        };
        let opts_arg = CallArgument {
            value: Some(serde_json::json!({
                "mode": "ai",
                "refPrefix": prefix,
                "depth": opts.max_depth,
                "track": if opts.diff { "observe" } else { "" },
            })),
            ..Default::default()
        };
        let raw = manager
            .call_injected(
                frame_id,
                "incrementalAriaSnapshot",
                vec![node_arg, opts_arg],
                true,
            )
            .await?;
        // by-value → result.value 是 {full, incremental?, iframeRefs, iframeDepths}。
        let value = raw.get("value").cloned().ok_or_else(|| {
            InjectError::Protocol(format!("incrementalAriaSnapshot result missing value: {raw}"))
        })?;
        let snap: FrameSnapshot = serde_json::from_value(value).map_err(|e| {
            InjectError::Protocol(format!("parse FrameSnapshot: {e}"))
        })?;
        Ok(Some(snap))
    }

    /// 建 iframe→子帧 路由表：`child_frame_id → (parent_frame_id, parent_iframe_ref)`。
    ///
    /// 对每个**已观测**子帧，`DOM.getFrameOwner(childFrameId)` → owner iframe 的 backendNodeId →
    /// `DOM.resolveNode(backendNodeId, executionContextId=parent_ctx)` → 父 utility world 的 objectId →
    /// `callFunctionOn(读 this._ariaRef.ref)` 拿父帧给该 iframe 元素分配的 ref。best-effort：
    /// 任一步失败该子帧不缝合（仍作为独立帧出现在输出，但不内联）。
    /// **D1**：用传入 active tab 句柄（主帧 id + 注入管线 context 反查）。
    async fn build_iframe_routing(
        &self,
        handles: &TabHandles,
        frames: &[ObservedFrame],
    ) -> HashMap<String, (String, String)> {
        let mut parent_of: HashMap<String, (String, String)> = HashMap::new();
        // frame_id → seq → prefix，用于在父帧的 iframe_refs 里确认归属（也可仅用 ref 字符串）。
        let frame_by_id: HashMap<&str, &ObservedFrame> =
            frames.iter().map(|f| (f.frame_id.as_str(), f)).collect();

        for child in frames {
            // 主帧无 owner iframe，跳过。
            if child.frame_id == handles.main_frame_id {
                continue;
            }
            if let Some((parent_fid, iref)) =
                self.resolve_owner_iframe_ref(handles, child, &frame_by_id).await
            {
                parent_of.insert(child.frame_id.clone(), (parent_fid, iref));
            }
        }
        parent_of
    }

    /// 解析单个子帧的 owner iframe ref（见 [`Self::build_iframe_routing`]）。任一步失败 → None。
    /// **D1**：用传入 active tab 句柄的注入管线做父帧 utility context 反查。
    ///
    /// **owner iframe 元素属于父帧的 target，不属子帧自己的 target**：
    /// - 同进程 iframe：父帧与子帧共用 page session，owner 在 page session。
    /// - 跨进程 OOPIF：owner `<iframe>` 占位元素在**父 target 的渲染进程**里，子帧另起独立
    ///   session；在 OOPIF **自身 session** 上 `getFrameOwner(自身根帧)` 必报 `-32000
    ///   "Frame ... does not belong to the target"`（实测，见 PLATFORM-VERIFICATION「OOPIF 缝合」）。
    ///
    /// 故对每个**候选父帧**，在**该候选父帧的 session** 上发 `getFrameOwner(childFrameId)`：
    /// 只有真父帧的 target 持有该 child 的 owner 元素 → 成功返 backendNodeId；非父帧报错跳过。
    /// 再 `resolveNode(backendNodeId, ctx=父 utility)` + 读 `_ariaRef.ref`，并以「ref ∈ 父帧
    /// iframe_refs」二次确认归属（防同进程下他帧 backendNodeId 误配）。
    ///
    /// 局限（已知、graceful degrade）：**嵌套 OOPIF**（OOPIF 内再嵌跨站 OOPIF）的父帧是中间
    /// OOPIF，其 utility context 不在 `handles.injection`（page session 管线）里 → `context_id_for`
    /// 取不到 → 该子帧不内联（仍作独立帧出现）。一级 OOPIF（父=主帧/page session）完整缝合。
    async fn resolve_owner_iframe_ref(
        &self,
        handles: &TabHandles,
        child: &ObservedFrame,
        frame_by_id: &HashMap<&str, &ObservedFrame>,
    ) -> Option<(String, String)> {
        for (pfid, pframe) in frame_by_id.iter() {
            if *pfid == child.frame_id {
                continue;
            }
            // a) getFrameOwner 在**候选父帧的 session** 上发——owner iframe 元素属父 target。childFrameId
            //    不属该 target（非真父 / OOPIF 自身 session）时报 -32000 → 跳过。同进程下所有帧共用
            //    page session,故任一候选都返同一 owner backendNodeId,靠下方 _ariaRef.ref 归属确认。
            let Ok(owner) = self
                .conn
                .send::<GetFrameOwnerParams>(
                    &pframe.session_id,
                    &GetFrameOwnerParams::new(child.frame_id.clone()),
                )
                .await
            else {
                continue;
            };
            let Some(backend_node_id) = owner.get("backendNodeId").and_then(|v| v.as_i64()) else {
                continue;
            };
            // b) 父帧 utility context（page session 管线反查；嵌套 OOPIF 父帧取不到 → 跳过,见 doc 局限）。
            let Ok(parent_ctx) = handles.injection.context_id_for(pframe.frame_id.as_str()) else {
                continue;
            };
            // c) resolveNode 到父帧 utility world。
            let resolve = ResolveNodeParams {
                node_id: None,
                backend_node_id: Some(
                    chromiumoxide::cdp::browser_protocol::dom::BackendNodeId::new(backend_node_id),
                ),
                object_group: None,
                execution_context_id: Some(ExecutionContextId::new(parent_ctx)),
            };
            let Ok(resolved) = self
                .conn
                .send::<ResolveNodeParams>(&pframe.session_id, &resolve)
                .await
            else {
                continue;
            };
            let Some(obj_id) = resolved
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            // d) 读该 iframe 元素被父帧 incrementalAriaSnapshot 分配的 _ariaRef.ref。
            let mut call = CallFunctionOnParams::new(
                "function() { return this && this._ariaRef ? this._ariaRef.ref : null; }"
                    .to_string(),
            );
            call.object_id = Some(RemoteObjectId::new(obj_id.to_string()));
            call.return_by_value = Some(true);
            let Ok(call_res) = self
                .conn
                .send::<CallFunctionOnParams>(&pframe.session_id, &call)
                .await
            else {
                continue;
            };
            if let Some(reff) = call_res
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
            {
                // 确认该 ref 确在父帧的 iframe_refs 里（防误配）。
                if pframe.snapshot.iframe_refs.iter().any(|r| r == reff) {
                    return Some((pframe.frame_id.clone(), reff.to_string()));
                }
            }
        }
        None
    }

    /// 解析缝合后 YAML 里每行的 `[ref=...]`，填 [`RefTable`] + 产出 [`ElementEntry`] 列表。
    /// ref 的归属帧（session_id/frame_id/frame_seq）按 ref 的 `f<seq>` 前缀回查 frames。
    fn parse_refs_into_table(
        &self,
        frames: &[ObservedFrame],
        stitched: &str,
        table: &mut RefTable,
    ) -> Vec<ElementEntry> {
        // seq → (session_id, frame_id)。
        let by_seq: HashMap<u32, (&str, &str)> = frames
            .iter()
            .map(|f| (f.seq, (f.session_id.as_str(), f.frame_id.as_str())))
            .collect();
        let mut entries = Vec::new();
        for line in stitched.lines() {
            let Some(reff) = parse_ref_token(line) else {
                continue;
            };
            // ref = f<seq>e<n>：抽 seq 定位帧。
            let Some(seq) = parse_seq_from_ref(&reff) else {
                continue;
            };
            let (role, name) = parse_role_name(line);
            let (session_id, frame_id) = by_seq
                .get(&seq)
                .map(|(s, f)| (s.to_string(), f.to_string()))
                .unwrap_or_default();
            table.insert(
                &reff,
                RefRecord {
                    session_id,
                    frame_id,
                    full_ref: reff.clone(),
                    role: role.clone(),
                    name: name.clone(),
                },
            );
            entries.push(ElementEntry {
                r#ref: reff,
                role,
                name,
                frame_seq: seq,
            });
        }
        entries
    }

    // ── act 反查（P2 命脉，actionability.rs）需要的内部访问器 ──────────────────────
    // resolve_ref_to_object / release_act_group 据 RefRecord.session_id 选注入管线、并报
    // NodeStale 时带当前代际。这些访问器把 CdpBackend 的私有字段以受控只读面暴露给同 crate 的
    // actionability 模块（避免把字段全 pub）。
    //
    // **D1 结构改造**：per-tab 字段下放进 TabRecord 后，这些访问器**不再返引用**（字段在
    // `tabs` 锁后的 HashMap 值里），改为**异步返克隆出的 owned 值**——经 active_tab_handles 短暂锁
    // tabs/active_target 克隆出句柄后立即释放（不跨 await 持 tabs 锁）。active tab 缺失 → 返 Err
    // （绝不 panic）。conn / next_act_seq 非 per-tab，保留同步引用/原子语义。

    /// active tab 的 page sessionId（actionability 据此判 RefRecord 属主帧还是 OOPIF 子帧）。
    /// `pub`：act 反查测试 / facade 构造 RefRecord 路由时需要。**D1：async 返 owned**（active tab
    /// 缺失 → Err）。
    pub async fn page_session_id(&self) -> Result<String, BrowserError> {
        Ok(self.active_tab_handles().await?.session_id)
    }

    /// active tab 的主帧 frameId（== page targetId，CDP 约定）。`pub`：同上，反查路由 / 测试构造记录用。
    /// **D1：async 返 owned**。
    pub async fn main_frame_id(&self) -> Result<String, BrowserError> {
        Ok(self.active_tab_handles().await?.main_frame_id)
    }

    /// active tab 的注入管线（同进程帧的 ref 反查走它；`Clone` 共享 Arc 缓存）。**D1：async 返克隆**。
    pub(crate) async fn injection_manager(&self) -> Result<InjectionManager, BrowserError> {
        Ok(self.active_tab_handles().await?.injection)
    }

    /// 底层 CDP 连接（B5 输入合成 / 后续 act 经它发裸 `Input.*` / `DOM.getContentQuads` 命令）。
    /// `pub(crate)`：input.rs 的输入合成方法据此把 active session 喂给收 `&Connection` 的自由
    /// 函数；不对外暴露（外部经 act facade 用，非裸连接）。**非 per-tab，保留同步引用**。
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 取 active tab 里某 OOPIF 子 session 的注入管线（克隆出句柄，锁外用）。未 arm/已 detach /
    /// active tab 缺失 → None。**D1：经 active tab 的 oopif_managers 解引用**。
    pub(crate) async fn oopif_manager_for(&self, session_id: &str) -> Option<InjectionManager> {
        let handles = self.active_tab_handles().await.ok()?;
        handles
            .oopif_managers
            .lock()
            .await
            .get(session_id)
            .map(|entry| entry.manager.clone())
    }

    /// active tab 当前（最近一次 observe 的）ref 表代际，供 actionability 报
    /// [`BrowserError::NodeStale`] 时带上。还没 observe 过（表为空）/ active tab 缺失 → 0（哨兵代际，
    /// 语义「任何 ref 都 stale」）。**D1：经 active tab 的 ref_table 解引用**。
    pub(crate) async fn current_generation(&self) -> u64 {
        let Ok(handles) = self.active_tab_handles().await else {
            return 0;
        };
        handles
            .ref_table
            .lock()
            .await
            .as_ref()
            .map(|t| t.generation().0)
            .unwrap_or(0)
    }

    /// **取下一个 act objectGroup 序号**（C1）：`fetch_add(1, Relaxed)` 返回当前值并自增。每次
    /// `act` 调一次，拼成本动作的 objectGroup `act-<seq>`，保证连续/并发动作的句柄组互不串味。
    /// **非 per-tab（全局），保留原子语义**。
    pub(crate) fn next_act_seq(&self) -> u64 {
        self.act_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// active tab 的 ref 表锁的 **Arc 克隆**（actionability 层① 在当前代际表里 `resolve(llm_ref)` 取
    /// [`crate::aria_ref::RefRecord`]）。返 Arc（非引用）让 actionability 在同一临界区内既读 generation
    /// 又 resolve（避免两次锁的 TOCTOU：observe 可能在两次锁之间翻新代际），且**不**让调用方跨 await
    /// 持 `tabs` 锁——clone 出 Arc 后独立锁它。active tab 缺失 → Err。**D1：async 返 per-tab ref_table 的 Arc**。
    pub(crate) async fn ref_table_lock(
        &self,
    ) -> Result<std::sync::Arc<AsyncMutex<Option<RefTable>>>, BrowserError> {
        Ok(self.active_tab_handles().await?.ref_table)
    }

    /// **[仅测试支持]** 在主 page 的**默认（页面）world** `Runtime.evaluate` 一段副作用脚本，用于
    /// 集成测试在 observe 之后**改 DOM 活元素状态**（如把某元素 `display:none` 制造「ref 已分配但
    /// 现已不可见」场景，验证 actionability 五检查的 `visible` 判定）。`#[doc(hidden)]`：非产品 API，
    /// 仅 `tests/integration_act.rs` 等用；走默认 world（而非 utility world）以便直接操作页面 DOM。
    /// 返回 `result.result` RemoteObject（by-value）。失败/JS 异常返 `Err`（绝不 panic）。
    /// **D1：经 active tab 的 session 发**。
    #[doc(hidden)]
    pub async fn __eval_page_world_for_test(
        &self,
        expression: &str,
    ) -> Result<serde_json::Value, BrowserError> {
        let session = self.active_tab_handles().await?.session_id;
        let mut params = EvaluateParams::new(expression.to_string());
        params.return_by_value = Some(true);
        params.await_promise = Some(false);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!("test eval threw: {ex}")));
        }
        Ok(result.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// **[仅测试支持]** 同 [`Self::__eval_page_world_for_test`]，但 `awaitPromise = true`——
    /// 用于 async JS 表达式（返回 Promise 的 IIFE）。等 Promise resolve 后返回 by-value 结果。
    #[doc(hidden)]
    pub async fn __eval_page_world_await_for_test(
        &self,
        expression: &str,
    ) -> Result<serde_json::Value, BrowserError> {
        let session = self.active_tab_handles().await?.session_id;
        let mut params = EvaluateParams::new(expression.to_string());
        params.return_by_value = Some(true);
        params.await_promise = Some(true);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!("test eval threw: {ex}")));
        }
        Ok(result.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// **[仅测试支持]** 关闭 active tab 的 page target（`Target.closeTarget{targetId}`，发到根 browser
    /// session），模拟「用户关掉标签页」——CDP 随之发 `Target.detachedFromTarget`（sessionId == 本 page
    /// session）。B6 集成测试用它触发 detach 事件源，验证 [`Self::arm_act_abort`] →
    /// `progress.abort(PageClosed)` → 进行中的 [`crate::actions::run_act_with_retry`] 立即以
    /// `TargetClosed` 返回（远早于 deadline）。`#[doc(hidden)]`：非产品 API，仅 `tests/integration_act.rs`
    /// 用。失败返 `Err`（绝不 panic）。**D1：关 active tab 的 target**。
    #[doc(hidden)]
    pub async fn __close_page_target_for_test(&self) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::target::CloseTargetParams;
        let target_id = self.active_tab_handles().await?.target_id;
        let params = CloseTargetParams::new(target_id);
        self.conn
            .send::<CloseTargetParams>(ROOT_SESSION, &params)
            .await
            .map_err(map_transport_err)?;
        Ok(())
    }

    /// **[仅测试支持]** 读回引擎构造期注入的出口防火墙配置（P3-G1 注入链验证）。`firewall_loop`
    /// 在后台任务里消费该配置、无法从外部直接观测；本 accessor 读回**与 loop 同值**的快照
    /// （[`Self::firewall_config`]），使 `#[ignore]` 集成测试能断言「自定义 FirewallConfig 真的注入到
    /// 了引擎」而非被硬编码 `default()` 吞掉。`#[doc(hidden)]`：非产品 API，仅集成测试用。
    #[doc(hidden)]
    pub fn firewall_config_for_test(&self) -> crate::firewall::FirewallConfig {
        // P3-D1：FirewallConfig 不再 Copy（含 Vec 域名策略字段）→ clone 返回。
        self.firewall_config.clone()
    }

    /// **[仅测试支持]** 当前 browser 里 `type=="page"` 的 target 总数（经 `Target.getTargets`）。
    /// 用于断言「启动后**恰好一个**受控 page」——验证 `--no-startup-window` 消除了命令行冗余
    /// about:blank 启动标签（旧行为 = 命令行 about:blank + createTarget 受控页 = **2** 个 page；
    /// 新行为 = 仅 createTarget 受控页 = **1** 个 page）。`#[doc(hidden)]`：非产品 API。
    #[doc(hidden)]
    pub async fn page_target_count_for_test(&self) -> Result<usize, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
        let raw = self
            .conn
            .send::<GetTargetsParams>(ROOT_SESSION, &GetTargetsParams { filter: None })
            .await
            .map_err(map_transport_err)?;
        let count = raw
            .get("targetInfos")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|ti| ti.get("type").and_then(|v| v.as_str()) == Some("page"))
                    .count()
            })
            .unwrap_or(0);
        Ok(count)
    }

    /// **OOPIF 验证 seam**：active tab 当前已 arm 的跨进程 OOPIF 子 session 数（`oopif_managers` 长度）。
    /// 真跨源 iframe（Chrome site-isolation 把它另起 `type=="iframe"` 子 session）才 >0;同进程 iframe
    /// （同源 / `srcdoc`）不另起子 session,恒 0。供 `integration_oopif` 断言「跨源 OOPIF 子 session 真被
    /// arm」（`spawn_oopif_arm_loop` 接线在真 http 多源页才走得到,离线 file:// 触发不到）。
    pub async fn oopif_session_count_for_test(&self) -> usize {
        match self.active_tab_handles().await {
            Ok(handles) => handles.oopif_managers.lock().await.len(),
            Err(_) => 0,
        }
    }

    /// **W4b：捕获默认 browser context 的全部 cookie → storage_state**（DESIGN §17）。
    ///
    /// 用 **`Storage.getCookies`**（不传 `browserContextId`）取**默认 browser context** 的所有
    /// cookie（**全字段保真**：CHIPS partitionKey + sameSite + domain/path/expires/httpOnly/secure +
    /// priority/sourceScheme/sourcePort），序列化进 [`crate::storage_state::StorageState`]。
    ///
    /// 失败 → [`BrowserError`]（绝不 panic）。
    pub async fn capture_cookies(&self) -> Result<crate::storage_state::StorageState, BrowserError> {
        let params = StorageGetCookiesParams::default();
        let raw = self
            .conn
            .send::<StorageGetCookiesParams>(ROOT_SESSION, &params)
            .await
            .map_err(map_transport_err)?;
        // 反序列化回包的 `cookies` 数组（Vec<network::Cookie>）。缺键 / 空 → 空（无 cookie 是合法态）。
        let cookies: Vec<chromiumoxide::cdp::browser_protocol::network::Cookie> = raw
            .get("cookies")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| {
                BrowserError::Other(format!("Storage.getCookies response parse failed: {e}"))
            })?
            .unwrap_or_default();
        Ok(crate::storage_state::StorageState::from_cdp_cookies(cookies))
    }

    /// **持久登录：组合捕获当前登录态**（[`Self::capture_cookies`] 全域 cookie +
    /// [`Self::capture_local_storage`] 当前 origin 的 localStorage + [`Self::capture_index_db`]
    /// best-effort）。cookie 是登录态主载体（全域采全）;localStorage/IndexedDB origin-bound,
    /// 只采当前 active tab 那个 origin。localStorage 采不到（about:blank / 无 location）→ 只带 cookie。
    /// IndexedDB 采集失败 → best-effort 忽略（`index_db=None`），绝不因此让整体 capture 失败。
    pub async fn capture_storage_state(
        &self,
    ) -> Result<crate::storage_state::StorageState, BrowserError> {
        let mut state = self.capture_cookies().await?;
        if let Some(mut origin_storage) = self.capture_local_storage().await? {
            // best-effort IndexedDB for the same origin; a failure must not sink the capture.
            origin_storage.index_db = self.capture_index_db().await.ok().flatten();
            state.local_storage.push(origin_storage);
        }
        Ok(state)
    }

    /// **W4b：把 storage_state 的 cookie 灌进默认 browser context（恢复登录态）**（DESIGN §17）。
    ///
    /// 用 **`Storage.setCookies`**（不传 `browserContextId`）把 [`crate::storage_state::StorageState`]
    /// 的 cookie 转成 `Network.CookieParam`（partitionKey/sameSite **原样灌**）写进**默认 context**——
    /// 恢复跨会话登录态。命令发到**根 browser session**（ROOT_SESSION）。
    ///
    /// **幂等/可重入**：`Storage.setCookies` 按 (name,domain,path,partitionKey) upsert，重复灌同一份
    /// storage_state 不产生重复 cookie。空 cookie 数组 → no-op（无登录态可灌）。
    ///
    /// 失败 → [`BrowserError`]（绝不 panic）。
    pub async fn restore_cookies(
        &self,
        state: &crate::storage_state::StorageState,
    ) -> Result<(), BrowserError> {
        let cookies = state.to_cookie_params();
        // 空 → no-op（setCookies 灌空数组无意义，且 cookies 字段 skip_serializing_if Vec::is_empty）。
        if cookies.is_empty() {
            return Ok(());
        }
        let params = StorageSetCookiesParams::new(cookies);
        self.conn
            .send::<StorageSetCookiesParams>(ROOT_SESSION, &params)
            .await
            .map_err(map_transport_err)?;
        Ok(())
    }

    /// **W4c：捕获**当前页面 origin 的 **localStorage** → 一个 [`crate::storage_state::OriginStorage`]
    /// （origin-bound，DESIGN §17）。
    ///
    /// **origin-bound 现实**：localStorage 按 origin 分区（同源策略）——一个文档只能读到**自己 origin**
    /// 的 localStorage，无法跨 origin 枚举。故捕获只能取**当前 active tab 页面已加载的那个 origin**
    /// 的 localStorage（caller 先 navigate 到目标 origin，再 capture）。这与 cookie 的 `Storage.getCookies`
    /// （能按 browserContextId 取全 context cookie）不同——localStorage 没有「按 context 取全 origin」的
    /// CDP 面，必须 per-origin 在页面上下文采。
    ///
    /// 注入脚本 `(()=>{ ... Object.entries(localStorage) ... })()`（默认 page world `Runtime.evaluate`，
    /// by-value）——返回 `{ origin, items:[[k,v],...] }`。`file://` 等 opaque origin（`location.origin`
    /// 形如 `"null"` / `"file://"`）也照样采（其 localStorage 仍是该文档的）。localStorage 访问被禁
    /// （如某些 sandbox / disabled storage）→ try/catch 兜底返回空 items（绝不 panic / 不抛）。
    ///
    /// 返回 `Ok(None)` 当当前页面**无可采 origin**（无 location）；否则 `Ok(Some(OriginStorage))`
    /// （items 可能为空 = 该 origin 无 localStorage 项，仍是合法快照）。IndexedDB best-effort（TODO）不采
    /// （`index_db=None`）。**经 active tab 的 page session 发**（D1）。
    pub async fn capture_local_storage(
        &self,
    ) -> Result<Option<crate::storage_state::OriginStorage>, BrowserError> {
        // 采当前页面 origin + localStorage 全键值（默认 page world；try/catch 兜底 storage 不可用）。
        // 返回 by-value `{origin, items:[[k,v],...]}`；无 location → origin 为空串。
        let script = "(() => { try { \
            const origin = (location && location.origin) ? location.origin : ''; \
            const items = []; \
            for (let i = 0; i < localStorage.length; i++) { \
                const k = localStorage.key(i); \
                items.push([k, localStorage.getItem(k)]); \
            } \
            return { origin, items }; \
        } catch (e) { return { origin: (location && location.origin) || '', items: [] }; } })()";
        let session = self.active_tab_handles().await?.session_id;
        let mut params = EvaluateParams::new(script.to_string());
        params.return_by_value = Some(true);
        params.await_promise = Some(false);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!(
                "capture_local_storage eval threw: {ex}"
            )));
        }
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let origin = value
            .get("origin")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // 无 origin（about:blank / 无 location）→ 无可采 origin。
        if origin.is_empty() {
            return Ok(None);
        }
        let mut items = Vec::new();
        if let Some(arr) = value.get("items").and_then(|v| v.as_array()) {
            for pair in arr {
                if let Some(p) = pair.as_array()
                    && let (Some(k), Some(v)) =
                        (p.first().and_then(|x| x.as_str()), p.get(1).and_then(|x| x.as_str()))
                {
                    items.push((k.to_string(), v.to_string()));
                }
            }
        }
        Ok(Some(crate::storage_state::OriginStorage::new_local_storage(
            origin, items,
        )))
    }

    /// **IndexedDB capture**：采集当前页面 origin 的所有 IndexedDB 数据库 →
    /// [`crate::storage_state::IndexedDbDump`]（origin-bound，DESIGN §17）。
    ///
    /// 注入一段 async JS（utility world 不需要，用默认 page world + `awaitPromise:true`）：
    /// - 枚举 `indexedDB.databases()` 获取所有 DB 名 + 版本。
    /// - 对每个 DB：`indexedDB.open(name, version)` → 遍历 `objectStoreNames` → 对每个 store
    ///   `getAll()` 取全部记录 + 读 `keyPath`/`autoIncrement`。
    /// - 二进制值（ArrayBuffer/typed array）→ base64 哨兵 `{"__b64__":"..."}`。
    /// - 返回结构化 JSON → 映射到 `IndexedDbDump`。
    ///
    /// `indexedDB.databases()` 在部分旧 Chrome (<72) 不可用——此时返回 `Ok(None)`（优雅降级）。
    /// opaque origin（`data:`/`file://`）的 Chrome 行为不稳定——若 `databases()` 失败返回 `Ok(None)`。
    ///
    /// 返回 `Ok(None)` = 当前 origin 无 IndexedDB（或不可枚举）；`Ok(Some(dump))` = 成功采集。
    pub async fn capture_index_db(
        &self,
    ) -> Result<Option<crate::storage_state::IndexedDbDump>, BrowserError> {
        // Collector JS: enumerate databases, open each, read all object stores + records.
        // Binary values (ArrayBuffer, typed arrays) are encoded as {"__b64__":"<base64>"}.
        let script = r#"(async () => {
            if (!indexedDB || !indexedDB.databases) return null;
            let dbs;
            try { dbs = await indexedDB.databases(); } catch(e) { return null; }
            if (!dbs || dbs.length === 0) return { databases: [] };

            function toBase64(buffer) {
                const bytes = new Uint8Array(buffer);
                let binary = '';
                for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
                return btoa(binary);
            }

            function encodeValue(val) {
                if (val === null || val === undefined) return val;
                if (val instanceof ArrayBuffer) return {"__b64__": toBase64(val)};
                if (ArrayBuffer.isView(val)) return {"__b64__": toBase64(val.buffer)};
                if (val instanceof Blob) return {"__b64__": ""};
                if (Array.isArray(val)) return val.map(encodeValue);
                if (typeof val === 'object' && val !== null) {
                    const out = {};
                    for (const [k, v] of Object.entries(val)) out[k] = encodeValue(v);
                    return out;
                }
                return val;
            }

            const result = [];
            for (const dbInfo of dbs) {
                try {
                    const db = await new Promise((resolve, reject) => {
                        const req = indexedDB.open(dbInfo.name, dbInfo.version);
                        req.onsuccess = () => resolve(req.result);
                        req.onerror = () => reject(req.error);
                        req.onupgradeneeded = () => {};
                    });
                    const stores = [];
                    for (const storeName of db.objectStoreNames) {
                        try {
                            const tx = db.transaction(storeName, "readonly");
                            const store = tx.objectStore(storeName);
                            const keyPath = store.keyPath;
                            const autoIncrement = store.autoIncrement;
                            const records = await new Promise((resolve, reject) => {
                                const req = store.getAll();
                                req.onsuccess = () => resolve(req.result);
                                req.onerror = () => reject(req.error);
                            });
                            stores.push({
                                name: storeName,
                                keyPath: typeof keyPath === 'string' ? keyPath : null,
                                autoIncrement: !!autoIncrement,
                                records: records.map(encodeValue)
                            });
                        } catch(e) { /* skip unreadable store */ }
                    }
                    db.close();
                    result.push({ name: dbInfo.name, version: dbInfo.version, stores });
                } catch(e) { /* skip unopenable db */ }
            }
            return { databases: result };
        })()"#;

        let session = self.active_tab_handles().await?.session_id;
        let mut params = EvaluateParams::new(script.to_string());
        params.return_by_value = Some(true);
        params.await_promise = Some(true);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            // JS 异常 → 不可捕获的 origin / 不支持 indexedDB.databases()。优雅降级。
            tracing::debug!(
                target: "nomi_browser_engine::storage_state",
                "capture_index_db eval exception (graceful degradation): {ex}"
            );
            return Ok(None);
        }
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // null / undefined → 无 IndexedDB（或 databases() 不可用）。
        if value.is_null() {
            return Ok(None);
        }

        // 解析 JS 返回的 { databases: [...] } 结构。
        let databases_val = value.get("databases").cloned().unwrap_or(serde_json::Value::Null);
        let databases_arr = match databases_val.as_array() {
            Some(arr) => arr,
            None => return Ok(None),
        };

        let mut databases = Vec::new();
        for db_val in databases_arr {
            let name = db_val
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = db_val
                .get("version")
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            let stores_arr = db_val.get("stores").and_then(|v| v.as_array());
            let mut stores = Vec::new();
            if let Some(arr) = stores_arr {
                for store_val in arr {
                    let store_name = store_val
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let key_path = store_val
                        .get("keyPath")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let auto_increment = store_val
                        .get("autoIncrement")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let records = store_val
                        .get("records")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    stores.push(crate::storage_state::IdbStore {
                        name: store_name,
                        key_path,
                        auto_increment,
                        records,
                    });
                }
            }
            databases.push(crate::storage_state::IdbDatabase {
                name,
                version,
                stores,
            });
        }

        if databases.is_empty() {
            return Ok(Some(crate::storage_state::IndexedDbDump::default()));
        }
        Ok(Some(crate::storage_state::IndexedDbDump { databases }))
    }

    /// **W4c：恢复 localStorage（origin-bound 注入）**——把 [`crate::storage_state::StorageState`] 里
    /// **匹配当前页面 origin** 的那个 [`crate::storage_state::OriginStorage`] 的键值，经
    /// `localStorage.setItem` 灌进当前页面（DESIGN §17 / 裁决⑥）。
    ///
    /// **origin-bound 注入的现实 + 本实现的边界（重要）**：
    /// localStorage 写入也受同源策略约束——只能写**当前文档 origin** 的 localStorage。一份 storage_state
    /// 可能含**多个 origin** 的 localStorage（用户在多站登录）；要把每个 origin 的 localStorage 都灌回，
    /// 严格做法是 DESIGN §17 的「**伪空 HTML 导航技巧**」：对每个 origin，用 `Fetch` 拦截器把该 origin 的
    /// 一个 URL 伪造成空 HTML 响应（避免真实网络/登录墙污染）→ `navigate(origin)` → 注入 `setItem`。
    /// 但本引擎的 `Fetch` 域已被**出口防火墙 loop**（[`super::cdp::spawn_fetch_firewall_loop`]）独占，
    /// 在 restore 期临时插一套伪 HTML 拦截会与 firewall loop 竞争同一 `Fetch.requestPaused` 流——这是个
    /// 真实的架构耦合点。
    ///
    /// 故 **W4c 采用「caller-navigated origin-bound 注入」**：本方法只把 **storage_state 中 origin ==
    /// 当前页面 origin** 的那一份 localStorage 注入当前页面（caller 先 `navigate(origin)` 再调本方法）。
    /// 多 origin 恢复 = caller 对每个 origin「navigate → restore」一轮（与捕获对称）。这把 origin-bound
    /// 注入做对（绝不跨 origin 误写），且不与 firewall loop 抢 `Fetch` 流；**伪空 HTML 自动遍历全 origin**
    /// 留作后续增强（见下方 TODO，需引擎层与 firewall loop 协调 `Fetch` 拦截，或改用一次性
    /// `addScriptToEvaluateOnNewDocument` origin-bound storageScript）。
    ///
    /// 行为：取当前页面 `location.origin` → 在 `state.local_storage` 找 origin 相等的项 → 逐键 `setItem`
    /// （存在则覆盖，幂等）。无匹配 origin（state 里没有当前页面 origin 的 localStorage）→ no-op（`Ok`）。
    /// 注入脚本经默认 page world `Runtime.evaluate`；JS 抛异常 → `Err`（绝不 panic）。**经 active tab 的
    /// page session 发**（D1）。
    // TODO(W4-followup): 伪空 HTML 导航自动遍历 state.local_storage 全 origin（DESIGN §17）——需与
    // firewall loop 协调 Fetch 拦截，或用 origin-bound addScriptToEvaluateOnNewDocument storageScript。
    pub async fn restore_local_storage(
        &self,
        state: &crate::storage_state::StorageState,
    ) -> Result<(), BrowserError> {
        // 无 localStorage 可恢复 → no-op。
        if state.local_storage.is_empty() {
            return Ok(());
        }
        let session = self.active_tab_handles().await?.session_id;
        // 取当前页面 origin（origin-bound：只灌匹配 origin 的那份）。
        let origin = {
            let mut p = EvaluateParams::new(
                "(() => (location && location.origin) ? location.origin : '')()".to_string(),
            );
            p.return_by_value = Some(true);
            let r = self
                .conn
                .send::<EvaluateParams>(&session, &p)
                .await
                .map_err(map_transport_err)?;
            r.get("result")
                .and_then(|x| x.get("value"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        // 在 state 里找 origin == 当前页面 origin 的那份 localStorage（origin-bound）。
        let Some(origin_storage) = state.local_storage.iter().find(|o| o.origin == origin) else {
            // state 里没有当前页面 origin 的 localStorage（caller 还没 navigate 到目标 origin，
            // 或该 origin 无 localStorage 需恢复）→ no-op（不跨 origin 误写）。
            return Ok(());
        };
        if origin_storage.local_storage.is_empty() {
            return Ok(());
        }
        // 把键值数组序列化进脚本（JSON 安全编码 key/value——含引号/反斜杠/换行都不破坏脚本）。
        let pairs: Vec<[&str; 2]> = origin_storage
            .local_storage
            .iter()
            .map(|i| [i.name.as_str(), i.value.as_str()])
            .collect();
        let pairs_json = serde_json::to_string(&pairs)
            .map_err(|e| BrowserError::Other(format!("serialize localStorage pairs: {e}")))?;
        // 注入：逐键 setItem（覆盖即幂等）。try/catch 兜底 storage 不可用（返回失败原因供诊断）。
        let script = format!(
            "(() => {{ try {{ const pairs = {pairs_json}; \
             for (const [k, v] of pairs) {{ localStorage.setItem(k, v); }} \
             return true; \
             }} catch (e) {{ throw new Error('localStorage.setItem failed: ' + e); }} }})()"
        );
        let mut params = EvaluateParams::new(script);
        params.return_by_value = Some(true);
        params.await_promise = Some(false);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!(
                "restore_local_storage eval threw: {ex}"
            )));
        }
        Ok(())
    }

    /// **IndexedDB restore（origin-bound 写回）**——把 [`crate::storage_state::StorageState`] 中
    /// **匹配当前页面 origin** 的 [`crate::storage_state::IndexedDbDump`] 恢复到当前页面的
    /// IndexedDB（origin-bound，DESIGN §17）。
    ///
    /// 行为：取当前页面 `location.origin` → 在 `state.local_storage` 中找 origin 相等且
    /// `index_db = Some(dump)` 的项 → 对 dump 中每个数据库：`indexedDB.open(name, version)` 创建
    /// （onupgradeneeded 中建 objectStore）→ 对每个 store `put` 全部 records（base64 哨兵
    /// `{"__b64__":"..."}` 解码回 ArrayBuffer）。
    ///
    /// **origin-bound**：只恢复 origin == 当前页面 origin 的那份 IDB（caller 先 navigate 到
    /// 目标 origin）。无匹配 origin / 无 index_db → no-op（`Ok`）。
    ///
    /// 注入的 restore JS 经默认 page world `Runtime.evaluate`（`awaitPromise:true`）；JS 抛异常
    /// → `Err`（绝不 panic）。**经 active tab 的 page session 发**。
    pub async fn restore_index_db(
        &self,
        state: &crate::storage_state::StorageState,
    ) -> Result<(), BrowserError> {
        if state.local_storage.is_empty() {
            return Ok(());
        }
        let session = self.active_tab_handles().await?.session_id;
        // 取当前页面 origin。
        let origin = {
            let mut p = EvaluateParams::new(
                "(() => (location && location.origin) ? location.origin : '')()".to_string(),
            );
            p.return_by_value = Some(true);
            let r = self
                .conn
                .send::<EvaluateParams>(&session, &p)
                .await
                .map_err(map_transport_err)?;
            r.get("result")
                .and_then(|x| x.get("value"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        // 找 origin 匹配且有 index_db 的 OriginStorage。
        let Some(origin_storage) = state.local_storage.iter().find(|o| o.origin == origin) else {
            return Ok(());
        };
        let Some(dump) = &origin_storage.index_db else {
            return Ok(());
        };
        if dump.databases.is_empty() {
            return Ok(());
        }

        // 序列化 dump 为 JSON 注入到 restore JS。
        let dump_json = serde_json::to_string(dump)
            .map_err(|e| BrowserError::Other(format!("serialize IndexedDbDump: {e}")))?;

        // Restore JS: for each DB, open with version (triggering upgradeneeded to create stores),
        // then put all records. Decode __b64__ sentinels back to ArrayBuffer.
        let script = format!(
            r#"(async () => {{
            const dump = {dump_json};

            function decodeValue(val) {{
                if (val === null || val === undefined) return val;
                if (Array.isArray(val)) return val.map(decodeValue);
                if (typeof val === 'object' && val !== null) {{
                    if (val.__b64__ !== undefined) {{
                        const binaryStr = atob(val.__b64__);
                        const bytes = new Uint8Array(binaryStr.length);
                        for (let i = 0; i < binaryStr.length; i++) bytes[i] = binaryStr.charCodeAt(i);
                        return bytes.buffer;
                    }}
                    const out = {{}};
                    for (const [k, v] of Object.entries(val)) out[k] = decodeValue(v);
                    return out;
                }}
                return val;
            }}

            for (const dbInfo of dump.databases) {{
                const db = await new Promise((resolve, reject) => {{
                    const req = indexedDB.open(dbInfo.name, dbInfo.version);
                    req.onupgradeneeded = (e) => {{
                        const db = e.target.result;
                        for (const storeInfo of dbInfo.stores) {{
                            if (!db.objectStoreNames.contains(storeInfo.name)) {{
                                const opts = {{}};
                                if (storeInfo.keyPath) opts.keyPath = storeInfo.keyPath;
                                if (storeInfo.autoIncrement) opts.autoIncrement = true;
                                db.createObjectStore(storeInfo.name, opts);
                            }}
                        }}
                    }};
                    req.onsuccess = () => resolve(req.result);
                    req.onerror = () => reject(req.error);
                }});

                for (const storeInfo of dbInfo.stores) {{
                    if (!db.objectStoreNames.contains(storeInfo.name)) continue;
                    const tx = db.transaction(storeInfo.name, "readwrite");
                    const store = tx.objectStore(storeInfo.name);
                    for (const record of storeInfo.records) {{
                        store.put(decodeValue(record));
                    }}
                    await new Promise((resolve, reject) => {{
                        tx.oncomplete = resolve;
                        tx.onerror = () => reject(tx.error);
                    }});
                }}
                db.close();
            }}
            return "ok";
        }})()"#
        );

        let mut params = EvaluateParams::new(script);
        params.return_by_value = Some(true);
        params.await_promise = Some(true);
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!(
                "restore_index_db eval threw: {ex}"
            )));
        }
        Ok(())
    }

    /// **多 origin localStorage + IndexedDB 自动遍历恢复**——无需 caller 逐 origin 手动 navigate，
    /// 一次调用自动遍历 [`crate::storage_state::StorageState`] 中所有 origin 的 localStorage
    /// （+ IndexedDB）并恢复到对应 origin。
    ///
    /// # 为什么用 `Page.addScriptToEvaluateOnNewDocument` 而不是 `Fetch` 拦截
    ///
    /// 本引擎的出口防火墙 loop（[`super::cdp::spawn_fetch_firewall_loop`]）**独占** CDP 的
    /// `Fetch.requestPaused` 事件通道——任何 `Fetch.enable` / 对 `requestPaused` 的第二个监听
    /// 都会与防火墙 loop 竞争事件分发，导致合法请求被意外 fail / 安全策略被绕过。
    ///
    /// 因此，多 origin 恢复采用以下 Fetch-free 方案：
    /// 1. 对每个 origin，用 `Page.addScriptToEvaluateOnNewDocument` 注册一段 origin-guard 脚本：
    ///    脚本内部检查 `location.origin === targetOrigin`，仅匹配时执行 `setItem` 注入。
    ///    该 CDP 方法**不拦截网络请求**——它只在**新文档创建时**（before scripts）在页面上下文注入
    ///    一段 JS，完全不碰 Fetch 通道，与防火墙 loop 零冲突。
    /// 2. 然后 `Page.navigate` 到该 origin（Chrome 发起真实网络请求——正常走防火墙审批），
    ///    navigate 完成后注入的脚本已在页面 load 前执行完毕（localStorage 就位）。
    /// 3. 对有 IndexedDB 的 origin，在 navigate 完成后再 `Runtime.evaluate`（async）恢复 IDB。
    /// 4. 全部 origin 恢复完毕后，`RemoveScriptToEvaluateOnNewDocument` 清理注册的脚本。
    ///
    /// **关键**：`addScriptToEvaluateOnNewDocument` 是 origin-agnostic 注册（全 origin 都会触发），
    /// 所以脚本内部必须做 `location.origin === xxx` 守卫，防止 navigate 到其他页面时误触。
    pub async fn restore_all_origins(
        &self,
        state: &crate::storage_state::StorageState,
    ) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::{
            AddScriptToEvaluateOnNewDocumentParams, RemoveScriptToEvaluateOnNewDocumentParams,
        };

        if state.local_storage.is_empty() {
            return Ok(());
        }

        let session = self.active_tab_handles().await?.session_id;
        let mut script_ids: Vec<String> = Vec::new();

        for origin_storage in &state.local_storage {
            if origin_storage.local_storage.is_empty() && origin_storage.index_db.is_none() {
                continue;
            }

            let target_origin = &origin_storage.origin;

            // ── 1. Register addScriptToEvaluateOnNewDocument for localStorage ──
            // Only register if there are localStorage items to restore.
            let mut script_id_for_this_origin: Option<String> = None;
            if !origin_storage.local_storage.is_empty() {
                let pairs: Vec<[&str; 2]> = origin_storage
                    .local_storage
                    .iter()
                    .map(|i| [i.name.as_str(), i.value.as_str()])
                    .collect();
                let pairs_json = serde_json::to_string(&pairs).map_err(|e| {
                    BrowserError::Other(format!("serialize localStorage pairs: {e}"))
                })?;
                let origin_json = serde_json::to_string(target_origin).map_err(|e| {
                    BrowserError::Other(format!("serialize origin: {e}"))
                })?;

                // Origin-guarded script: only runs if location.origin matches.
                let inject_script = format!(
                    "(() => {{ \
                        if (location.origin !== {origin_json}) return; \
                        try {{ \
                            const pairs = {pairs_json}; \
                            for (const [k, v] of pairs) localStorage.setItem(k, v); \
                        }} catch(e) {{}} \
                    }})()"
                );

                let add_params = AddScriptToEvaluateOnNewDocumentParams {
                    source: inject_script,
                    world_name: None,
                    include_command_line_api: None,
                    run_immediately: None,
                };
                let resp = self
                    .conn
                    .send::<AddScriptToEvaluateOnNewDocumentParams>(&session, &add_params)
                    .await
                    .map_err(map_transport_err)?;
                // Extract the script identifier for cleanup.
                if let Some(id) = resp.get("identifier").and_then(|v| v.as_str()) {
                    script_ids.push(id.to_string());
                    script_id_for_this_origin = Some(id.to_string());
                }
            }

            // ── 2. Navigate to origin (triggers the registered script on load) ──
            let nav_params = NavigateParams::new(target_origin.clone());
            let _ = self
                .conn
                .send::<NavigateParams>(&session, &nav_params)
                .await
                .map_err(map_transport_err)?;

            // Wait briefly for the page to load (give localStorage script time to run).
            // The addScriptToEvaluateOnNewDocument runs before page scripts, so by the time
            // navigate resolves, localStorage is already set. But we want to ensure the
            // page actually loaded. A simple wait for loadEventFired or just a short delay.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // ── 3. Restore IndexedDB for this origin (if any) ──
            if origin_storage.index_db.is_some() {
                // Build a mini state with just this origin for restore_index_db.
                let mini_state = crate::storage_state::StorageState {
                    cookies: vec![],
                    local_storage: vec![origin_storage.clone()],
                };
                // restore_index_db reads the current page origin and matches — we just navigated there.
                self.restore_index_db(&mini_state).await?;
            }

            // ── 4. Remove the localStorage script for this origin (no longer needed) ──
            if let Some(id) = script_id_for_this_origin {
                use chromiumoxide::cdp::browser_protocol::page::ScriptIdentifier;
                let remove_params =
                    RemoveScriptToEvaluateOnNewDocumentParams::new(ScriptIdentifier::new(id));
                // Best-effort removal — failure doesn't break correctness.
                let _ = self
                    .conn
                    .send_may_fail::<RemoveScriptToEvaluateOnNewDocumentParams>(
                        &session,
                        &remove_params,
                    )
                    .await;
            }
        }

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// D3：tab 动作（tabs / switch_tab / close_tab / open_link_new_tab）。
// 全程沿用 active_tab_handles 的锁模式——短临界区锁 `tabs`/`active_target` 取/改后立即释放，
// **绝不**跨 await 持 `tabs` 锁（CDP 命令的 await 都在锁外）。switch 是纯逻辑指针切换
// （正确性不依赖 activateTarget/bringToFront，headless 弱——DESIGN:213/不变量⑱）。
// ═══════════════════════════════════════════════════════════════════════════

impl CdpBackend {
    /// **[纯逻辑/锁内] 在 `tabs` 注册表里按 last4 / 全 id 解析一个 tab_id**：短暂锁 `tabs` 取所有 key →
    /// [`crate::tabs::resolve_last4_among`] 判唯一/撞号/零命中 → 唯一返完整 targetId；撞号 → `Blocked`
    /// （让 LLM 用更长前缀）；零命中 → `Blocked`（无此 tab）。**不进浏览器**（只查注册表）。
    async fn resolve_tab_id(&self, tab_id: &str) -> Result<String, BrowserError> {
        use crate::tabs::{resolve_last4_among, Last4Match};
        let ids: Vec<String> = {
            let guard = self.tabs.lock().await;
            guard.keys().cloned().collect()
        };
        match resolve_last4_among(tab_id, ids.iter().map(|s| s.as_str())) {
            Last4Match::Unique(full) => Ok(full),
            Last4Match::Ambiguous(hits) => Err(BrowserError::Blocked {
                reason: format!(
                    "tab id {tab_id:?} is ambiguous (matches {}); use a longer id",
                    hits.len()
                ),
            }),
            Last4Match::NotFound => Err(BrowserError::Blocked {
                reason: format!("no open tab matches id {tab_id:?}; call tabs to list open tabs"),
            }),
        }
    }

    /// **tabs 列表动作**（D3，DESIGN §13，Info 级只读）：枚举当前所有纳管标签 → (last4, url, title,
    /// is_active) → 渲染成对 LLM 文案。url/title 经 `Target.getTargets`（一次取全量 targetInfo），按本
    /// 注册表的 tab key 过滤（只列我们纳管的 page，不含 OOPIF/SW/其它 browser target）。
    pub async fn act_tabs(&self) -> Result<ActResult, BrowserError> {
        use crate::tabs::{last4, render_tab_list, TabListItem};
        use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;

        // 注册表里的 tab 集合 + 当前 active（短临界区取后释放锁）。
        let (managed, active): (Vec<String>, String) = {
            let tabs = self.tabs.lock().await;
            let managed = tabs.keys().cloned().collect();
            let active = self.active_target.lock().await.clone();
            (managed, active)
        };

        // getTargets 取 url/title（发到根 browser session）。失败 → best-effort 空 info（url/title 留空）。
        let mut info: HashMap<String, (String, String)> = HashMap::new();
        if let Ok(raw) = self
            .conn
            .send::<GetTargetsParams>(ROOT_SESSION, &GetTargetsParams { filter: None })
            .await
            && let Some(arr) = raw.get("targetInfos").and_then(|v| v.as_array())
        {
            for ti in arr {
                let Some(tid) = ti.get("targetId").and_then(|v| v.as_str()) else {
                    continue;
                };
                let url = ti.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let title = ti.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                info.insert(tid.to_string(), (url, title));
            }
        }

        // 只列我们纳管的 tab（注册表 key），按 last4 排序保稳定输出。
        let mut items: Vec<TabListItem> = managed
            .iter()
            .map(|tid| {
                let (url, title) = info.get(tid).cloned().unwrap_or_default();
                TabListItem {
                    last4: last4(tid),
                    target_id: tid.clone(),
                    url,
                    title,
                    is_active: *tid == active,
                }
            })
            .collect();
        items.sort_by(|a, b| a.last4.cmp(&b.last4));

        Ok(ActResult {
            message: render_tab_list(&items),
            effect: Effect {
                changed: false,
                before_anchor: None,
                after_anchor: None,
            },
            success: true,
        })
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 调试捕获读取动作（只读，读 per-tab 缓冲并脱敏序列化）
    // ═══════════════════════════════════════════════════════════════════════

    /// `get_console_logs` 动作：读取 active tab 的 console 缓冲，脱敏后返回。
    pub async fn act_get_console_logs(&self) -> Result<ActResult, BrowserError> {
        let handles = self.active_tab_handles().await?;
        let snap = crate::debug_capture::DebugSnapshot::from_buffers(&handles.debug);
        let secrets = self.known_secret_values.lock().unwrap_or_else(|e| e.into_inner());
        let message = crate::debug_capture::serialize_console_for_llm(&snap.console, &secrets);
        Ok(ActResult {
            message,
            effect: Effect { changed: false, before_anchor: None, after_anchor: None },
            success: true,
        })
    }

    /// `get_page_errors` 动作：读取 active tab 的 errors 缓冲，脱敏后返回。
    pub async fn act_get_page_errors(&self) -> Result<ActResult, BrowserError> {
        let handles = self.active_tab_handles().await?;
        let snap = crate::debug_capture::DebugSnapshot::from_buffers(&handles.debug);
        let secrets = self.known_secret_values.lock().unwrap_or_else(|e| e.into_inner());
        let message = crate::debug_capture::serialize_errors_for_llm(&snap.errors, &secrets);
        Ok(ActResult {
            message,
            effect: Effect { changed: false, before_anchor: None, after_anchor: None },
            success: true,
        })
    }

    /// `get_network_log` 动作：读取 active tab 的 network 缓冲，脱敏后返回。
    pub async fn act_get_network_log(&self, include_bodies: bool) -> Result<ActResult, BrowserError> {
        let handles = self.active_tab_handles().await?;
        let snap = crate::debug_capture::DebugSnapshot::from_buffers(&handles.debug);
        let secrets = self.known_secret_values.lock().unwrap_or_else(|e| e.into_inner());
        let message = crate::debug_capture::serialize_network_for_llm(&snap.network, include_bodies, &secrets);
        Ok(ActResult {
            message,
            effect: Effect { changed: false, before_anchor: None, after_anchor: None },
            success: true,
        })
    }

    /// **switch_tab 动作**（D3，DESIGN §13/不变量⑱）：解析 tab_id（撞号→Blocked）→ 把 `active_target`
    /// 指向该 targetId（**逻辑指针切换**）。切换后 observe/act 自动作用新 active tab（经 active_tab_handles）。
    /// headful 下**额外** best-effort `Target.activateTarget` 把它前置（**正确性不依赖它**——headless 弱）。
    pub async fn act_switch_tab(&self, tab_id: &str) -> Result<ActResult, BrowserError> {
        use crate::tabs::last4;
        use chromiumoxide::cdp::browser_protocol::target::ActivateTargetParams;

        let target_id = self.resolve_tab_id(tab_id).await?;
        // 逻辑指针切换（短临界区）。
        {
            let mut active = self.active_target.lock().await;
            *active = target_id.clone();
        }
        // headful best-effort 前置（不影响正确性；headless 弱，失败忽略）。
        if self.headful {
            let _ = self
                .conn
                .send::<ActivateTargetParams>(ROOT_SESSION, &ActivateTargetParams::new(target_id.clone()))
                .await;
        }
        let l4 = last4(&target_id);
        Ok(ActResult {
            message: format!("switched to tab [{l4}]; observe to see its content"),
            effect: Effect {
                changed: true,
                before_anchor: None,
                after_anchor: Some(serde_json::json!({ "active_tab": l4 })),
            },
            success: true,
        })
    }

    /// **close_tab 动作**（D3，DESIGN §13）：解析 tab_id → `Target.closeTarget` → 从 `tabs` 移除
    /// 该 [`TabRecord`] → **显式 `.abort()` 其 `_inject_loop`/`_oopif_loop`**（D1 评审要点：drop 是
    /// detach 非 abort，全局连接仍存活时这俩循环不会靠 `RecvError::Closed` 退出，必须显式 abort 防泄漏
    /// 空转）→ 若关的是 active tab：重选一个剩余 tab 作 active（无剩余 → `SessionLost`，并对进行中操作
    /// `Progress::abort(PageClosed)`）。
    ///
    /// `parent` 是动作的 [`Progress`] 作用域：关掉 active tab 时若有进行中操作绑该 tab，本动作的 parent
    /// 取消会经 token 层级传播（这里对 parent abort(PageClosed) 兑现「关 active → 进行中操作立即取消」）。
    pub async fn close_tab_impl(
        &self,
        tab_id: &str,
        parent: &Progress,
    ) -> Result<ActResult, BrowserError> {
        use crate::tabs::last4;
        use chromiumoxide::cdp::browser_protocol::target::CloseTargetParams;

        let target_id = self.resolve_tab_id(tab_id).await?;
        let l4 = last4(&target_id);

        // 1) closeTarget（发到根 browser session）。CDP 随之发 detachedFromTarget。
        self.conn
            .send::<CloseTargetParams>(ROOT_SESSION, &CloseTargetParams::new(target_id.clone()))
            .await
            .map_err(map_transport_err)?;

        // 2) 从 tabs 移除 TabRecord + **显式 abort 其两个后台循环**（防泄漏空转）。短临界区。
        let was_active;
        let reselected: Option<String>;
        {
            let mut tabs = self.tabs.lock().await;
            if let Some(record) = tabs.remove(&target_id) {
                // D1 评审要点：drop(record) 只 detach JoinHandle，全局连接仍存活时循环不退出 → 必须显式 abort。
                record._inject_loop.abort();
                record._oopif_loop.abort();
                record._debug_loop.abort();
            }
            // 3) 若关的是 active tab：重选一个剩余 tab 作 active。
            let mut active = self.active_target.lock().await;
            was_active = *active == target_id;
            if was_active {
                // 取任一剩余 tab（按 key 排序取最小，确定性）。
                reselected = tabs.keys().min().cloned();
                if let Some(ref new_active) = reselected {
                    *active = new_active.clone();
                }
            } else {
                reselected = None;
            }
        }

        if was_active {
            // 关 active：对进行中操作（绑该 tab 的 act）发 abort(PageClosed)——立即取消而非白等。
            parent.abort(AbortReason::PageClosed);
            match reselected {
                Some(new_id) => {
                    let new_l4 = last4(&new_id);
                    Ok(ActResult {
                        message: format!(
                            "closed active tab [{l4}]; active is now [{new_l4}] (observe to see it)"
                        ),
                        effect: Effect {
                            changed: true,
                            before_anchor: None,
                            after_anchor: Some(serde_json::json!({ "active_tab": new_l4 })),
                        },
                        success: true,
                    })
                }
                // 关了最后一个 tab：没有可作用的 page 了（session lost）。
                None => Err(BrowserError::SessionLost { recoverable: true }),
            }
        } else {
            // 关的是非 active tab：active 不变，本动作不影响进行中操作。
            Ok(ActResult {
                message: format!("closed tab [{l4}]; active tab unchanged"),
                effect: Effect {
                    changed: true,
                    before_anchor: None,
                    after_anchor: None,
                },
                success: true,
            })
        }
    }

    /// **open_link_new_tab 动作**（D3，DESIGN §13）：`Target.createTarget{url, background:true}`——
    /// **background 不抢焦点**。新 page 的 attachedToTarget 会被 [`spawn_tab_discovery_loop`] 收编 arm
    /// 入 `tabs`（不改 active）。返回新 tab 的 last4（让 LLM 显式 switch）。
    ///
    /// **active 不变**：本动作只开 tab、不切换；observe/act 仍作用原 active tab，直到 LLM 显式 switch_tab。
    pub async fn act_open_link_new_tab(&self, url: &str) -> Result<ActResult, BrowserError> {
        use crate::tabs::last4;

        // createTarget{url, background:true}（不抢焦点）。
        // 建在默认 browser context（browser identity 全局共享）。
        let mut params = CreateTargetParams::new(url);
        params.background = Some(true);
        let raw = self
            .conn
            .send::<CreateTargetParams>(ROOT_SESSION, &params)
            .await
            .map_err(map_transport_err)?;
        let new_tid = raw
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| BrowserError::Other("createTarget response missing targetId".into()))?
            .to_string();
        let l4 = last4(&new_tid);

        Ok(ActResult {
            message: format!(
                "opened {url} in a new tab [{l4}] (did not switch to it); use switch_tab [{l4}] to focus it"
            ),
            effect: Effect {
                // 开了新 tab（页面态有变），但 active tab 未变。
                changed: true,
                before_anchor: None,
                after_anchor: Some(serde_json::json!({ "new_tab": l4 })),
            },
            success: true,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// D4：history 导航（back/forward）+ reload（POST 页→IRREVERSIBLE 检测）+ switch_frame
// （active_frame 逻辑指针）。全经 active tab 句柄；settle **复用 D2 的 settle_after_trigger
// → run_settle**（零分叉，对齐 D4「别另写一套 settle」）；良性边界态（无更多历史）不报错。
// ═══════════════════════════════════════════════════════════════════════════

impl CdpBackend {
    /// **back/forward 动作**（D4，DESIGN §12）：`Page.getNavigationHistory` 取 currentIndex + entries
    /// → [`nav::history_target_index`] 算目标 entry 索引（**边界钳制**：首页 back / 末页 forward →
    /// `None`，**良性返「无更多历史」success=true 不报错、不 panic**）→ `Page.navigateToHistoryEntry`
    /// → **复用 D2 settle**（[`Self::settle_after_trigger`] → run_settle）等导航完成 → 返 [`NavResult`]。
    /// 经 active tab 的 page session + 主帧句柄。
    pub async fn act_history_nav(
        &self,
        direction: nav::HistoryNav,
    ) -> Result<ActResult, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::GetNavigationHistoryParams;

        let handles = self.active_tab_handles().await?;
        let session = handles.session_id.clone();
        let main_frame_id = handles.main_frame_id.clone();

        // 取导航历史（currentIndex + entries）。失败 → 上抛（传输错）。
        let history = self
            .conn
            .send::<GetNavigationHistoryParams>(&session, &GetNavigationHistoryParams::default())
            .await
            .map_err(map_transport_err)?;
        let current_index = history
            .get("currentIndex")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                BrowserError::Other("getNavigationHistory missing currentIndex".into())
            })?;
        let entries = history
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let dir_label = match direction {
            nav::HistoryNav::Back => "back",
            nav::HistoryNav::Forward => "forward",
        };

        // 边界钳制：算目标 entry 索引（首页 back / 末页 forward → None = 良性无更多历史）。
        let Some(target_idx) = nav::history_target_index(current_index, entries.len(), direction)
        else {
            // 良性：无更多历史 → success=true（changed=false），如实告知，**不报错、不 panic**。
            return Ok(ActResult {
                message: format!("no more history to go {dir_label} (already at the edge)"),
                effect: Effect {
                    changed: false,
                    before_anchor: None,
                    after_anchor: None,
                },
                success: true,
            });
        };

        // 取目标 entry 的 id（navigateToHistoryEntry 按 entryId 导航）。形状异常 → Other（不 panic）。
        let entry_id = entries
            .get(target_idx)
            .and_then(|e| e.get("id"))
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                BrowserError::Other(format!(
                    "navigation history entry {target_idx} missing id"
                ))
            })?;

        // 触发前的当前 url（redirect 归一化比较的「from」端——history 导航回到的目标即「应到」的页，
        // 与 final_url 同源 → 一般不算 redirect）。
        let before_url = self.current_url(&session).await.unwrap_or_default();

        // navigateToHistoryEntry + **复用 D2 settle**（settle_after_trigger → run_settle，零分叉）。
        let nav = self
            .settle_after_trigger(&session, &main_frame_id, &before_url, |conn, sess| async move {
                use chromiumoxide::cdp::browser_protocol::page::NavigateToHistoryEntryParams;
                conn.send::<NavigateToHistoryEntryParams>(
                    &sess,
                    &NavigateToHistoryEntryParams::new(entry_id),
                )
                .await
                .map_err(map_transport_err)?;
                Ok(())
            })
            .await?;

        Ok(ActResult {
            message: format!(
                "navigated {dir_label}; now at {} (load state: {}); re-observe to see the page",
                nav.final_url, nav.load_state
            ),
            effect: Effect {
                changed: true,
                before_anchor: Some(serde_json::Value::String(before_url)),
                after_anchor: Some(serde_json::json!({
                    "url": nav.final_url,
                    "load_state": nav.load_state.to_string(),
                    "http_status": nav.http_status,
                })),
            },
            success: true,
        })
    }

    /// **reload 动作**（D4，DESIGN §12 + 裁决⑧）：`Page.reload`（`ignoreCache=false`，普通刷新）→
    /// **复用 D2 settle**（[`Self::settle_after_trigger`] → run_settle）。
    ///
    /// **POST 页 → IRREVERSIBLE 检测（裁决⑧，D4 只检测/标记，不接 enforcement）**：reload 前查
    /// `Page.getNavigationHistory`，若当前 entry 的 `transitionType == form_submit`（POST 表单提交页，
    /// reload 会**重新提交表单**——重复下单/扣款/发消息），[`nav::current_entry_is_post`] 判 true，则：
    /// - 记一条 `tracing::warn` 标记（与 C2 press_key 的 IRREVERSIBLE 检测同范式）；
    /// - 在返回的 [`ActResult`] `effect.after_anchor` 带 `irreversible: true` 标志（供 E2/F1 接强制门读）；
    /// - **不**在此 enforce（不 hard-deny / 不要带外确认）——强制门是 E2/F1 的 facade 独立门职责。
    ///
    /// **可行性说明**：CDP 不直接给导航的 HTTP method；`transitionType==form_submit` 是最接近的可观测
    /// 信号（PW/browser-use 同此）。GET 表单也标 form_submit 会被**保守过判**为 irreversible（宁多确认
    /// 不漏判真 POST）；拿不到 transition（缺字段/形状陌生）→ **保守判非 irreversible**（不给普通页 reload
    /// 加确认门，与 spec「拿不准时不误判」一致）。
    pub async fn act_reload(&self) -> Result<ActResult, BrowserError> {
        use chromiumoxide::cdp::browser_protocol::page::{
            GetNavigationHistoryParams, ReloadParams,
        };

        let handles = self.active_tab_handles().await?;
        let session = handles.session_id.clone();
        let main_frame_id = handles.main_frame_id.clone();

        // POST 页检测（裁决⑧，D4 只检测）：查导航历史当前 entry 的 transitionType。best-effort——取
        // 不到历史 → 保守 false（不误判普通页 reload 为不可逆）。
        let irreversible = match self
            .conn
            .send::<GetNavigationHistoryParams>(&session, &GetNavigationHistoryParams::default())
            .await
        {
            Ok(history) => {
                let current_index = history.get("currentIndex").and_then(|v| v.as_i64()).unwrap_or(-1);
                let entries = history.get("entries").cloned().unwrap_or(serde_json::Value::Null);
                nav::current_entry_is_post(&entries, current_index)
            }
            Err(_) => false,
        };
        if irreversible {
            tracing::warn!(
                target: "nomi_browser_engine::backend::cdp",
                "reload detected IRREVERSIBLE (current page came from a POST form submit; \
                 reloading re-submits the form); TODO(E2/F1): wire fail-closed enforcement \
                 (D4 detection-only, not blocking)"
            );
        }

        let before_url = self.current_url(&session).await.unwrap_or_default();

        // Page.reload（ignoreCache=false 普通刷新）+ **复用 D2 settle**（settle_after_trigger →
        // run_settle）。redirect 比较「from」用触发前 url（reload 通常停在同 url → 不算 redirect）。
        let nav = self
            .settle_after_trigger(&session, &main_frame_id, &before_url, |conn, sess| async move {
                let params = ReloadParams::builder().ignore_cache(false).build();
                conn.send::<ReloadParams>(&sess, &params)
                    .await
                    .map_err(map_transport_err)?;
                Ok(())
            })
            .await?;

        let message = if irreversible {
            format!(
                "reloaded {} (load state: {}); NOTE: this page came from a form submission — \
                 reloading may re-submit it (irreversible); re-observe to see the page",
                nav.final_url, nav.load_state
            )
        } else {
            format!(
                "reloaded {} (load state: {}); re-observe to see the page",
                nav.final_url, nav.load_state
            )
        };

        Ok(ActResult {
            message,
            effect: Effect {
                changed: true,
                before_anchor: Some(serde_json::Value::String(before_url)),
                after_anchor: Some(serde_json::json!({
                    "url": nav.final_url,
                    "load_state": nav.load_state.to_string(),
                    "http_status": nav.http_status,
                    // IRREVERSIBLE 标志（D4 只标记；E2/F1 facade 独立门读它做强制门）。
                    "irreversible": irreversible,
                })),
            },
            success: true,
        })
    }

    /// **switch_frame 动作**（D4，DESIGN §13 语义采纳见下）：把后续**页面级动作**的默认作用域从主帧
    /// 切到给定 iframe 元素 ref 指向的**内容帧**（content frame）。
    ///
    /// **采纳的 DESIGN §13 语义**：DESIGN §13 把 `switch_frame` 列为导航类动作但未细化语义；§9 列其为
    /// Info/Exec。结合 PLAN D4「switch_frame（ref→frame session/world）」+ §7 句柄模型（ref-based 动作
    /// 经 `aria-ref=f<seq>e<n>` 本就跨帧工作、不受 active_frame 影响），最自洽的实现 = 设一个
    /// **active_frame 逻辑指针**（`Some((session_id, frame_id))` = 已切入某 iframe / `None` = 主帧/顶层），
    /// 让**页面级动作**（get_page_text / scroll(viewport) / press_key / find_elements 等无 element ref 的
    /// 动作）默认作用于该 iframe 而非主帧；ref-based 动作（click/type 经 ref 前缀路由到所属帧）**不受
    /// 影响**（本就跨帧）。这与 browser-use 的 switch_frame 语义一致（聚焦 LLM 的「当前帧」上下文）。
    ///
    /// **解析**：resolve ref（层①②③，须是页面里的 iframe 元素）→ 取其元素 objectId → `DOM.describeNode`
    /// 读该 iframe 元素的 `node.frameId`（= **内容帧** id；iframe 元素 node 的 frameId 即它承载的子文档帧）
    /// → 设 active_frame 指针为 `(该 iframe 所属 session, contentFrameId)`。非 iframe 元素（describeNode
    /// 无 frameId）→ success=false 如实（引导换 ref）。
    ///
    /// **切回主帧/顶层**：传特殊 ref `"main"` / `"top"` / 空串 → active_frame 置 `None`（页面级动作回主帧）。
    ///
    /// **D4 范围**：设指针 + 让 active_frame 影响**页面级动作**的 frame 解析（见 [`Self::active_page_frame`]
    /// 接入点）。注：D4 把指针接进 [`Self::main_frame_id`]/[`Self::page_session_id`] 的**页面级解析**
    /// （这俩是页面级动作取 frame 的入口）；同进程 iframe（同 page session）已可端到端验证，跨进程 OOPIF
    /// 切帧接线就位但离线 fixture 触发不到（`TODO(verify-oopif)`）。
    pub async fn act_switch_frame(&self, llm_ref: &str) -> Result<ActResult, BrowserError> {
        // 切回主帧/顶层：特殊 ref（main/top/空）→ active_frame 置 None。
        let trimmed = llm_ref.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("main") || trimmed.eq_ignore_ascii_case("top") {
            {
                let mut af = self.active_frame.lock().await;
                *af = None;
            }
            return Ok(ActResult {
                message: "switched back to the top/main frame; page-level actions now act on the main document".into(),
                effect: Effect {
                    changed: true,
                    before_anchor: None,
                    after_anchor: Some(serde_json::json!({ "active_frame": "main" })),
                },
                success: true,
            });
        }

        // resolve ref（层①②③）：拿活元素句柄 + 它所属帧（iframe 元素本身住在父帧）。
        let seq = self.next_act_seq();
        let rec = self.resolve_ref_record(llm_ref).await?;
        let handle = self.resolve_ref_to_object(&rec, seq).await?;

        // describeNode 读 iframe 元素的内容帧 id（node.frameId）。iframe 元素 node 的 frameId 即其承载的
        // 子文档帧；非 iframe 元素 → 无 frameId → 良性失败（引导换 ref）。在 iframe 元素所属 session 上发。
        let content_frame_id = self.iframe_content_frame_id(&rec.session_id, &handle.object_id).await;
        // 释放本次 resolve 的句柄组（switch_frame 不持续持有元素句柄）。
        self.release_act_group(&rec, seq).await;

        let Some(content_frame_id) = content_frame_id else {
            // 非 iframe 元素（无 contentFrame）→ success=false 如实（非报错，良性，引导换 ref）。
            return Ok(ActResult {
                message: format!(
                    "{llm_ref} is not an <iframe> (no content frame); switch_frame only works on iframe elements"
                ),
                effect: Effect {
                    changed: false,
                    before_anchor: None,
                    after_anchor: None,
                },
                success: false,
            });
        };

        // 设 active_frame 指针：页面级动作据此把默认作用域切到该 iframe（session 沿用 iframe 元素所属
        // session——同进程 iframe 与父帧同 page session；跨进程 OOPIF 子帧另起子 session，离线测不到）。
        {
            let mut af = self.active_frame.lock().await;
            *af = Some((rec.session_id.clone(), content_frame_id.clone()));
        }

        Ok(ActResult {
            message: format!(
                "switched into iframe {llm_ref}; page-level actions (get_page_text/scroll/find_elements/…) \
                 now act on that frame; re-observe to see its content"
            ),
            effect: Effect {
                changed: true,
                before_anchor: None,
                after_anchor: Some(serde_json::json!({ "active_frame": content_frame_id })),
            },
            success: true,
        })
    }

    /// **[运行时] 读一个 iframe 元素的内容帧 frameId**（switch_frame 解析）：`DOM.describeNode{objectId}`
    /// 返回该元素的 Node 描述，其中 `node.frameId` 对 **iframe 元素**即它承载的子文档帧 id。在元素所属
    /// `session` 上发。非 iframe 元素 → Node 无 frameId → `None`（良性，调用方引导换 ref）。任何 CDP/形状
    /// 失败 → `None`（best-effort，绝不 panic）。
    async fn iframe_content_frame_id(&self, session: &str, object_id: &str) -> Option<String> {
        use chromiumoxide::cdp::browser_protocol::dom::DescribeNodeParams;
        let params = DescribeNodeParams::builder()
            .object_id(RemoteObjectId::new(object_id.to_string()))
            .build();
        let result = self
            .conn
            .send::<DescribeNodeParams>(session, &params)
            .await
            .ok()?;
        // node.frameId（iframe 元素 → 内容帧 id；非 iframe → 缺该字段）。
        result
            .get("node")
            .and_then(|n| n.get("frameId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// **[运行时] 页面级动作的当前作用帧**（switch_frame 接入点）：若 active_frame 指针指向某 iframe
    /// （switch_frame 切入后），返该 iframe 的 `(session_id, frame_id)`；否则（`None` / 指向的帧已不在
    /// 当前 active tab）退到 active tab 的主帧 `(page_session, main_frame_id)`。
    ///
    /// **页面级动作**（get_page_text / scroll(viewport) / press_key / find_elements / cursor / scroll_to_text
    /// 等无 element ref 的动作）经此取作用帧——故 switch_frame 后它们作用于 iframe 而非主帧。**ref-based
    /// 动作不经此**（它们按 ref 的所属帧路由，本就跨帧，不受 active_frame 影响）。
    ///
    /// active_frame 指向的 frame 不再属于当前 active tab（切了 tab / 帧已 detach）→ 退主帧（保守，
    /// 不在错的 tab 上操作 stale 帧）。
    pub(crate) async fn active_page_frame(&self) -> Result<(String, String), BrowserError> {
        let handles = self.active_tab_handles().await?;
        let af = self.active_frame.lock().await.clone();
        if let Some((session, frame_id)) = af {
            // active_frame 的 session 必须是当前 active tab 的 page session（同进程 iframe）或其 OOPIF
            // 子 session——D4 范围：同进程 iframe 与主帧同 page session，故校验 session 匹配 active tab。
            // 跨 tab 切换后旧 active_frame 失效 → 退主帧。
            if session == handles.session_id {
                return Ok((session, frame_id));
            }
            // 也可能是 active tab 的 OOPIF 子 session（跨进程子帧；离线测不到，接线就位）。
            if handles.oopif_managers.lock().await.contains_key(&session) {
                return Ok((session, frame_id));
            }
            // 否则（切了 tab / 帧 detach）→ 退主帧（保守）。
        }
        Ok((handles.session_id, handles.main_frame_id))
    }

    /// **[运行时] 在「当前作用帧」的 world 里跑一段只读 `Runtime.evaluate`（by-value）**（D4 switch_frame
    /// 接入点：页面级只读动作经此取文本/状态，故 switch_frame 后作用于 iframe 而非主帧）。
    ///
    /// 作用帧由 [`Self::active_page_frame`] 决定：
    /// - **未 switch_frame（active_frame=None）/ 已退主帧**：在该 (page session) 的**默认 page world**
    ///   `evaluate`（无 context_id——与 D4 前行为完全一致，主帧 document）。
    /// - **已 switch_frame 切入 iframe**：在该 iframe 的 **utility-world contextId** `evaluate`
    ///   （isolated world 与页面 world 共享同一 DOM document，故 `document.body.innerText`/`location.href`
    ///   等读到的是**该 iframe 的文档**）。utility context 未就绪（导航中）→ 退默认 world（best-effort）。
    ///
    /// 返回 `result.result.value`（by-value）；JS 抛异常 → `Err(Other)`；CDP 失败 → 经 map_transport_err。
    /// 抽此单点让所有页面级只读 helper（get_page_text/scroll_to_text/text_present/count_pointer_cursor/
    /// focus_in_form/current_url）一致地受 active_frame 影响（switch_frame 一处改、全页面级动作生效）。
    pub(crate) async fn active_frame_eval(
        &self,
        expression: &str,
    ) -> Result<serde_json::Value, BrowserError> {
        let (session, frame_id) = self.active_page_frame().await?;
        let mut params = EvaluateParams::new(expression.to_string());
        params.return_by_value = Some(true);
        params.await_promise = Some(false);
        // 若 active_frame 指向非主帧 iframe，且其 utility context 已就绪 → 在该 context evaluate
        // （作用于 iframe 文档）。主帧 / context 未就绪 → 默认 page world（无 context_id，主文档）。
        let main_frame_id = self.active_tab_handles().await?.main_frame_id;
        if frame_id != main_frame_id
            && let Ok(injection) = self.injection_manager().await
            && let Ok(ctx) = injection.context_id_for(&frame_id)
        {
            params.context_id = Some(ExecutionContextId::new(ctx));
        }
        let result = self
            .conn
            .send::<EvaluateParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(BrowserError::Other(format!("eval threw: {ex}")));
        }
        Ok(result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// **Evaluate 分支**（E3，DESIGN §16「evaluate」/ 裁决⑨）：在页面上下文跑任意 JS——**最高危逃生舱**，
    /// 故是引擎默认最高门控。
    ///
    /// 门控逻辑（[`crate::evaluate::gate`]，**纯逻辑、不读 session_mode**——不变量⑧）：
    /// - **默认 OFF**：未显式开「全权模式」→ [`crate::evaluate::evaluate_off_error`]（`Unsupported{evaluate}`，
    ///   hint 讲清为何 off + 怎么开）。这是 default-deny——没有任何 session 默认能 evaluate。
    /// - **opt-in 全权**：用户显式 opt-in 全权（LIVE 读，F1 灌入 [`Self::evaluate_gate`]）+ 无持久登录 → 放行。
    /// - **与持久登录互斥**：全权 + 持久登录同开 → `Blocked`（互斥；持久登录灌着真实长期凭据，禁任意 JS）。
    /// - **持久登录下封死**：持久登录开启时 evaluate 强制 OFF（即便全权也被互斥拦下）。
    /// - **yolo 不豁免**：放行**只看全权开关**，不看 `SessionMode`——yolo/companion 无从豁免（不变量⑧）。
    ///
    /// 放行后：记一条**醒目审计**（[`crate::evaluate::audit_evaluate`]，script 只记**脱敏摘要**不记全文）→
    /// 在当前作用帧 [`Self::active_frame_eval`] 跑该脚本，返 by-value 结果。**前端醒目展示留 P3**。
    ///
    /// **绝不自动重试**（裁决⑧/⑨「IRREVERSIBLE 禁重试」镜像）：evaluate 不走 [`run_act_with_retry`] 退避，
    /// 单次执行——任意 JS 副作用不可逆，重试可能重复执行。
    pub(crate) async fn act_evaluate(&self, script: &str) -> Result<ActResult, BrowserError> {
        // 门控（纯逻辑，**不读 session_mode**）：默认 OFF / 全权 opt-in / 与持久登录互斥。
        let cfg = *self.evaluate_gate.lock().await;
        crate::evaluate::gate(&cfg)?;

        // 放行（仅全权 opt-in + 无持久登录）：记醒目审计（script 只记脱敏摘要不记全文）。
        let origin = self.act_current_url().await;
        crate::evaluate::audit_evaluate(script, origin.as_deref());

        // 在当前作用帧跑该脚本（单次，绝不重试）。JS 抛异常 → Err(Other)；CDP 失败 → 映射错误。
        let value = self.active_frame_eval(script).await?;
        Ok(ActResult {
            message: format!(
                "evaluated script in the page (full-power mode); result: {value}; re-observe to see any DOM changes"
            ),
            effect: Effect {
                // evaluate 任意 JS 可能改 DOM/导航——保守视作 changed（无法静态判定其副作用）。
                changed: true,
                before_anchor: None,
                after_anchor: Some(value),
            },
            success: true,
        })
    }

    // ═══════════════════════════════════════════════════════════════════════
    // F-actions CDP 原语（pub(crate)，供 actions.rs 的 act_upload_file / act_download /
    // act_save_as_pdf 编排调用）。这些方法持有 self.conn / self.download_dir / active tab 句柄等
    // **cdp 模块私有态**的访问权，故落在 cdp.rs；编排逻辑（skeleton/retry/RetryDecision）在 actions.rs。
    // ═══════════════════════════════════════════════════════════════════════

    /// **隔离下载目录绝对路径访问器**（F-actions：download 探测落点 / save_as_pdf 写入落点）。
    /// `Some` 当且仅当 E4 下载沙箱已挂；`None` = 纯引擎冒烟（无落点）。
    pub(crate) fn download_dir(&self) -> Option<&str> {
        self.download_dir.as_deref()
    }

    /// **SD-2 上传路径沙箱根访问器**。`Some` = per-pet workspace（upload 必须 in-sandbox）；
    /// `None` = 无 workspace（fail-closed：一律拒绝上传）。
    pub(crate) fn workspace_dir(&self) -> Option<&std::path::Path> {
        self.workspace_dir.as_deref()
    }

    /// **[运行时] DOM.setFileInputFiles（在 file input 元素上设置上传文件路径）**（upload_file 真执行）。
    /// `object_id` 是 resolve_ref 产出的 utility-world 元素句柄——DOM 域按 objectId 解析节点（跨 world，
    /// 与 [`Self::iframe_content_frame_id`] 的 describeNode 同范式）。元素不是 `<input type=file>` →
    /// CDP 回 error → 经 [`map_transport_err`] 成 `Other`（调用方判 Fatal）；节点 detach → CDP error。
    /// 在元素所属 `session` 上发（同进程 iframe 与父帧同 page session）。
    pub(crate) async fn set_file_input_files(
        &self,
        session: &str,
        object_id: &str,
        files: &[String],
    ) -> Result<(), BrowserError> {
        use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
        let params = SetFileInputFilesParams {
            files: files.to_vec(),
            node_id: None,
            backend_node_id: None,
            object_id: Some(RemoteObjectId::new(object_id.to_string())),
        };
        self.conn
            .send::<SetFileInputFilesParams>(session, &params)
            .await
            .map_err(map_transport_err)?;
        Ok(())
    }

    /// **[运行时] 读 file input 的 files 摘要**作 verify 锚点（upload_file）：`{count, first}`
    /// （files.length + 首文件名）。non-file 元素 / 异常 → None（best-effort）。**只读**。
    pub(crate) async fn act_read_file_input(
        &self,
        object_id: &str,
    ) -> Option<serde_json::Value> {
        let read_fn = "function() { \
             try { \
                 if (!this || this.tagName !== 'INPUT' || this.type !== 'file') return null; \
                 var f = this.files; \
                 var n = f ? f.length : 0; \
                 var first = (f && f.length > 0) ? f[0].name : null; \
                 return { count: n, first: first }; \
             } catch (e) { return null; } \
         }";
        let manager = self.injection_manager().await.ok()?;
        let result = manager.call_on_element(object_id, read_fn, true).await.ok()?;
        match result.get("value") {
            Some(v) if v.is_object() => Some(v.clone()),
            _ => None,
        }
    }

    /// **[运行时] 注入隐藏 `<a href=url download>` 并 click 触发下载**（download 选项 A）。在当前作用帧
    /// 的**默认 page world**（[`Self::active_frame_eval`]）跑——`a.click()` 在页面 world 当用户手势触发
    /// 下载（走 `Browser.setDownloadBehavior(allowAndName)` 沙箱 + downloadWillBegin/Progress 事件循环，
    /// E4 denylist/MOTW 全链生效）。url 经 JSON.stringify 安全内联。**不扰当前页**（创建游离 `<a>`，click
    /// 后立即移除）。异常 → 上抛（Fatal）。
    pub(crate) async fn trigger_anchor_download(&self, url: &str) -> Result<(), BrowserError> {
        let safe_url = serde_json::Value::String(url.to_string()).to_string();
        let expression = format!(
            "(() => {{ try {{ \
               const a = document.createElement('a'); \
               a.href = {safe_url}; \
               a.download = ''; \
               a.style.display = 'none'; \
               a.rel = 'noopener'; \
               document.body.appendChild(a); \
               a.click(); \
               a.remove(); \
               return true; \
             }} catch (e) {{ return false; }} }})()"
        );
        let value = self.active_frame_eval(&expression).await?;
        if value.as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(BrowserError::Other(
                "failed to inject the download trigger (page may block dynamic anchors)".into(),
            ))
        }
    }

    /// **[运行时] 轮询隔离 downloads 目录至出现新增已完成文件**（download verify）。`before` 是触发前的
    /// 文件名集；每 [`DOWNLOAD_POLL_INTERVAL`] 扫一次目录，找**不在 `before`、size>0、且非 chrome 中间态**
    /// （`.crdownload`/`.tmp`）的文件即视作落盘完成，返 `(name, size)`。短 deadline
    /// （[`DOWNLOAD_SETTLE_TIMEOUT`]）内无新增 → `None`（良性，调用方报 success=false）。
    pub(crate) async fn poll_download_landed(
        &self,
        dir: &str,
        before: &std::collections::HashSet<String>,
    ) -> Option<(String, u64)> {
        let deadline = tokio::time::Instant::now() + DOWNLOAD_SETTLE_TIMEOUT;
        loop {
            if let Some(found) = newest_completed_download(dir, before) {
                return Some(found);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(DOWNLOAD_POLL_INTERVAL).await;
        }
    }

    /// **[运行时] Page.printToPDF → 原始 PDF 字节**（save_as_pdf）。返回 base64-decode 后的 PDF bytes。
    /// 在 active tab 的 page session 上发（整页打印，非某帧）。headful 已实测可用（Chrome 149）；
    /// 某版本受限 / CDP 失败 → `Err`（绝不 panic）。
    pub(crate) async fn print_to_pdf(&self) -> Result<Vec<u8>, BrowserError> {
        let session = self.active_tab_handles().await?.session_id;
        // 默认参数：print_background=true（保留页面背景，更接近所见），其余默认（A4、不分页范围）。
        let params = PrintToPdfParams::builder().print_background(true).build();
        let result = self
            .conn
            .send::<PrintToPdfParams>(&session, &params)
            .await
            .map_err(map_transport_err)?;
        let pdf: PrintToPdfReturns = serde_json::from_value(result.clone())
            .map_err(|e| BrowserError::Other(format!("parse printToPDF response: {e}")))?;
        // `data` 是 base64（同 captureScreenshot）。
        let b64: &str = pdf.data.as_ref();
        decode_base64(b64).ok_or_else(|| BrowserError::Other("printToPDF returned non-base64 data".into()))
    }

}
pub struct ActAbortGuard {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ActAbortGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // 监听任务只读事件 + 调 abort，无须优雅关闭：直接 abort 取消（动作已结束，事件不再相关）。
            h.abort();
        }
    }
}

/// **[纯逻辑] 事件 params 的 `sessionId` 是否匹配目标 session**（detach/crash 接线判定）。
/// `Target.detachedFromTarget` / `Target.targetCrashed` 的 params 带 `sessionId`（标识哪个 target
/// 没了/崩了）；与本 page session 比对，命中即「本动作所在 page 没了」。抽纯函数便于单测形状解析。
fn event_session_matches(params: &serde_json::Value, target_session: &str) -> bool {
    params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s == target_session)
        .unwrap_or(false)
}

/// **[纯逻辑] 事件 params 的 `frameId` 是否匹配目标帧**（frame detach 接线判定）。
/// `Page.frameDetached` 的 params 带 `frameId`（被 detach 的帧）；与动作所在帧比对，命中即「本动作
/// 所在帧从树上 detach」。抽纯函数便于单测形状解析。
fn event_frame_matches(params: &serde_json::Value, target_frame: &str) -> bool {
    params
        .get("frameId")
        .and_then(|v| v.as_str())
        .map(|s| s == target_frame)
        .unwrap_or(false)
}

/// **navigate settle 的产物**（[`CdpBackend::run_settle`] 返回）：达到的 settle 状态 + 是否走了
/// SPA 软导航降级路径（软导航是良性态，不升级 networkidle）。
struct SettleOutcome {
    state: NavSettleState,
    soft_nav: bool,
}

/// settle 各 select 分支收 broadcast 事件的「是否拿到一条有效事件」判定（D2）。`Ok(_)` → true；
/// `Lagged`（订阅落后）→ true（当作收到一次，继续推进——宁可早一步也不卡死；具体语义由各 absorb
/// 函数对 params 解析兜底）；`Closed`（连接没了）→ false（让外层 select 转向超时/其它分支兜底，
/// 绝不 busy-loop）。这里把 `Lagged` 当 true 是因为生命周期里程碑（DCL/load）即便丢了具体那条，
/// 后续仍能靠超时阶梯 + history 查 url 兜底，不致命。
fn recv_ok(
    ev: Result<crate::transport::CdpEvent, tokio::sync::broadcast::error::RecvError>,
) -> bool {
    use tokio::sync::broadcast::error::RecvError;
    match ev {
        Ok(_) => true,
        Err(RecvError::Lagged(_)) => true,
        Err(RecvError::Closed) => false,
    }
}

/// 吸收一条 `Network.responseReceived`：若是主帧 Document 响应，填 `http_status`（首个命中为准——
/// 主文档响应只有一个；后续子资源/子帧响应被 [`nav::extract_main_doc_status`] 过滤掉）。
fn absorb_response(
    ev: Result<crate::transport::CdpEvent, tokio::sync::broadcast::error::RecvError>,
    main_frame_id: &str,
    http_status: &mut Option<u16>,
) {
    if let Ok(ev) = ev
        && http_status.is_none()
        && let Some(s) = nav::extract_main_doc_status(&ev.params, main_frame_id)
    {
        *http_status = Some(s);
    }
}

/// 吸收一条 `Network.requestWillBeSent`：按是否「重定向续发」（有 redirectResponse）+1 / 不变。
fn absorb_request(
    ev: Result<crate::transport::CdpEvent, tokio::sync::broadcast::error::RecvError>,
    inflight: &mut InflightCounter,
) {
    if let Ok(ev) = ev {
        inflight.on_request_will_be_sent(nav::request_is_redirect(&ev.params));
    }
}

/// 吸收一条 `Network.loadingFinished`：-1（钳零）。
fn absorb_finish(
    ev: Result<crate::transport::CdpEvent, tokio::sync::broadcast::error::RecvError>,
    inflight: &mut InflightCounter,
) {
    if ev.is_ok() {
        inflight.on_loading_finished();
    }
}

/// 吸收一条 `Network.loadingFailed`：-1（钳零）。
fn absorb_fail(
    ev: Result<crate::transport::CdpEvent, tokio::sync::broadcast::error::RecvError>,
    inflight: &mut InflightCounter,
) {
    if ev.is_ok() {
        inflight.on_loading_failed();
    }
}

/// 一个已观测帧：seq（拼 `f<seq>` 前缀）+ frame_id + 所属 session + 单帧快照。
struct ObservedFrame {
    seq: u32,
    frame_id: String,
    session_id: String,
    snapshot: FrameSnapshot,
}

/// D5：收集单帧 password 输入的 aria ref（同帧 utility world），追加进 `out`。
///
/// **fail-closed 契约**：返回 `true` 表示该帧 password 探测**失败**（`password_refs` 返 `Err`）。
/// 失败时无法精确知道哪些字段是 password，**不得**只 warn 放行——调用方据此对全部可编辑控件值
/// over-redact 兜底（见 [`CdpBackend::observe_impl`] step 6）。正常路径（`Ok`）返回 `false` 并把
/// 该帧 password ref 追加进 `out`，缝合后宿主侧精确抹其 value。
async fn collect_password_refs(
    manager: &InjectionManager,
    frame_id: &str,
    out: &mut Vec<String>,
) -> bool {
    match manager.password_refs(frame_id).await {
        Ok(refs) => {
            out.extend(refs);
            false
        }
        Err(e) => {
            tracing::warn!(
                target: "nomi_browser_engine::backend::cdp",
                frame_id = %frame_id, error = ?e,
                "collect password refs failed (D5: will fail-closed over-redact all editable values)"
            );
            true
        }
    }
}

/// 自 `frames[idx]` 起递归缝合：把每个「父=本帧」的子帧的（递归缝合后）full 内联进来。
/// `parent_of`: child_frame_id → (parent_frame_id, parent_iframe_ref)。无环（frameTree 是树）。
fn render_frame_recursive(
    frames: &[ObservedFrame],
    idx: usize,
    parent_of: &HashMap<String, (String, String)>,
) -> String {
    let me = &frames[idx];
    // 找所有「父帧 == 本帧」的子帧，递归渲染其 full，按 (iframe_ref, child_full) 收集。
    let mut children: Vec<(String, String)> = Vec::new();
    for (cidx, cf) in frames.iter().enumerate() {
        if let Some((pfid, iref)) = parent_of.get(&cf.frame_id)
            && *pfid == me.frame_id
        {
            let child_full = render_frame_recursive(frames, cidx, parent_of);
            children.push((iref.clone(), child_full));
        }
    }
    stitch(&me.snapshot, &children)
}

/// 单帧是否触及 depth 封顶（粗判：full 里出现缩进达 `(max_depth)*2` 空格的行——
/// renderAriaTree 在 depth==limit 时仍渲染该层但不再下钻，故粗判用于 truncated 标志）。
fn frame_hit_depth_limit(snap: &FrameSnapshot, max_depth: u32) -> bool {
    if max_depth == 0 {
        return false;
    }
    let limit_indent = (max_depth as usize) * 2;
    snap.full.lines().any(|l| {
        let lead = l.len() - l.trim_start().len();
        lead >= limit_indent
    })
}

/// 从 aria 行抽 `[ref=...]` 的 ref 值（不含 `[cursor=pointer]` 等后缀）。
fn parse_ref_token(line: &str) -> Option<String> {
    let start = line.find("[ref=")? + 5;
    let end = line[start..].find(']')? + start;
    Some(line[start..end].to_string())
}

/// 从 `f<seq>e<n>` 形态的 ref 抽 `<seq>`。形态不符返回 None。
fn parse_seq_from_ref(reff: &str) -> Option<u32> {
    let rest = reff.strip_prefix('f')?;
    let e_pos = rest.find('e')?;
    rest[..e_pos].parse::<u32>().ok()
}

/// 从 aria 行抽 (role, name)。行形如 `  - button "Submit order" [ref=f0e1]` 或 `- iframe [ref=f0e5]`。
/// role = `- ` 后到首个空格/引号/`[` 的 token；name = 首个 `"..."`（无引号串则空）。
fn parse_role_name(line: &str) -> (String, String) {
    let t = line.trim_start();
    let t = t.strip_prefix("- ").unwrap_or(t);
    // role：到首个 ' ' / '"' / '[' 为止。
    let role_end = t.find([' ', '"', '[']).unwrap_or(t.len());
    let role = t[..role_end].trim().to_string();
    // name：首个双引号包裹的串（aria 用 JSON.stringify，故内部转义按 JSON；这里取裸内容到下个未转义引号）。
    let name = extract_quoted(t).unwrap_or_default();
    (role, name)
}

/// 抽行内首个双引号包裹的串内容（尊重 `\"` 转义）。无引号串返回 None。
fn extract_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = s.find('"')? + 1;
    let mut out = String::new();
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '\\' && i + 1 < bytes.len() {
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if c == '"' {
            return Some(out);
        }
        out.push(c);
        i += 1;
    }
    None
}

/// 标准 base64 解码（CDP 截图 `data` 是干净的标准 base64）。用 workspace 的 `base64`
/// crate（与全仓惯例一致，免手写维护）。
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

// ═══════════════════════════════════════════════════════════════════════════
// F-actions：download 落盘探测 + save_as_pdf 输出路径（纯逻辑 free 函数，便于单测）。
// ═══════════════════════════════════════════════════════════════════════════

/// download verify 的落盘探测短超时（触发后等隔离 downloads 目录出现新增已完成文件的上限）。
/// 比 action 默认略宽（下载经网络 + 落盘异步），但远小于 nav 30s（避免整轮挂死）；超时即 success=false
/// 如实（良性：可能被红线取消 / url 无附件 / 仍在传）。
const DOWNLOAD_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// download 落盘探测的轮询间隔（每隔这么久扫一次目录）。
const DOWNLOAD_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// **[纯逻辑] 列一个目录下的「文件名」集合**（download 落盘探测的触发前基线）。目录不存在 / 读不了 →
/// 空集（best-effort，绝不 panic）。只收**文件**（非子目录）的文件名（`file_name()` 的 lossy 串）。
/// `pub(crate)`：actions.rs 的 act_download 取触发前基线用。
pub(crate) fn list_dir_files(dir: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return set;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            && let Some(name) = entry.file_name().to_str()
        {
            set.insert(name.to_string());
        }
    }
    set
}

/// **[纯逻辑] 在 downloads 目录里找一个「新增的、已完成的」下载文件**（download verify 单步探测）。
/// 「新增」= 文件名不在 `before` 基线集；「已完成」= size>0 且**非 chrome 中间态**（`.crdownload`
/// / `.tmp` 后缀是下载进行中的临时文件，不算落盘完成）。命中返 `(name, size)`（首个满足的）；无 → None。
/// best-effort：读目录 / 取元数据失败 → 跳过该项（绝不 panic）。
fn newest_completed_download(
    dir: &str,
    before: &std::collections::HashSet<String>,
) -> Option<(String, u64)> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        // 已存在（触发前就在）→ 非本次下载，跳过。
        if before.contains(&name) {
            continue;
        }
        // chrome 下载中间态（仍在传）→ 不算完成，跳过。
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".crdownload") || lower.ends_with(".tmp") {
            continue;
        }
        // size>0 才算落盘完成（0 字节多是刚创建占位）。
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size > 0 {
            return Some((name, size));
        }
    }
    None
}

/// **[纯逻辑] save_as_pdf 的输出文件路径**：`<downloads_dir>/page-<unix_ts_ms>.pdf`。时间戳（毫秒）
/// 防同会话多次 save_as_pdf 覆盖。系统时钟异常（早于 UNIX 纪元，几乎不可能）→ 退 `page-0.pdf`。
/// `pub(crate)`：actions.rs 的 act_save_as_pdf 算落点用。
pub(crate) fn pdf_output_path(downloads_dir: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::path::Path::new(downloads_dir).join(format!("page-{ts}.pdf"))
}

/// **E4 下载沙箱接线**：在**根 browser session** 挂 `Browser.setDownloadBehavior`。
///
/// `behavior = allowAndName`（让 chrome 用下载 GUID 命名落盘文件，规避同名覆盖/路径穿越攻击），
/// `downloadPath = <per-pet workspace>/downloads`（隔离落点，**绝不**用户真实 Downloads），
/// `eventsEnabled = true`（开 `downloadWillBegin`/`downloadProgress` 事件——MOTW 循环靠它知道
/// 落盘完成的 `filePath`）。
///
/// 浏览器级（ROOT_SESSION）：作用于默认 browser context 的所有 page，故对当前及后开标签页统一生效。
/// `browser_context_id: None` = 默认 context。
async fn set_download_behavior_sandbox(
    conn: &Connection,
    download_path: &str,
) -> Result<(), BrowserError> {
    let params = SetDownloadBehaviorParams {
        behavior: SetDownloadBehaviorBehavior::AllowAndName,
        browser_context_id: None,
        download_path: Some(download_path.to_string()),
        events_enabled: Some(true),
    };
    conn.send::<SetDownloadBehaviorParams>(ROOT_SESSION, &params)
        .await
        .map_err(map_transport_err)?;
    Ok(())
}

/// **E4 下载事件后台循环**：订阅 `Browser.downloadProgress`，对**完成**的下载在其落盘文件上打
/// Win MOTW（`Zone.Identifier` ADS）。
///
/// `downloadProgress` 的最后一次调用 `state=="completed"` 且（在桌面平台）`filePath` 给出落盘的
/// 实际路径（`allowAndName` 下是 `downloads/<GUID>`）。我们对该文件调
/// [`crate::download::write_motw`]——Windows 真写 ADS，mac/linux 空实现。**绝不**自动打开/启动文件。
///
/// best-effort：MOTW 是纵深防御附加层，写失败（非 NTFS / 文件已被移走）只 `debug` 不致命。连接关闭
/// （`RecvError::Closed`）→ 退出循环（backend Drop 关连接即触发）。
///
/// **E4 下载事件后台循环 + F1-sec 可执行下载红线 enforcement**。
///
/// 订阅两个事件：
/// 1. **`Browser.downloadWillBegin`**（F1-sec 接线点）：下载**发起**时即给出 `suggestedFilename`。
///    命中 [`crate::download::reject_executable_download`]（可执行/脚本 denylist）→ 立刻
///    `Browser.cancelDownload{guid}` **取消**该下载（fail-closed，**红线**——yolo/companion 也取消，
///    因为这道判定**不看 session_mode**：denylist 命中即拒，无放行参数，见 `reject_executable_download`
///    的红线语义）。这正是「可执行下载在红线会话也拒」的真实 enforcement（在落盘之前拦下）。
/// 2. **`Browser.downloadProgress`**：对**完成**（`state=="completed"`）的下载在其落盘文件打 Win MOTW
///    （`Zone.Identifier` ADS，Windows 真写 / mac-linux 空实现）。**绝不**自动打开/启动文件。
///
/// 二者互补：`downloadWillBegin` 在**发起**侧拦可执行（早于落盘）；`downloadProgress` 在**完成**侧对
/// 放行的非可执行下载打 MOTW。被取消的可执行下载不会走到 completed，故不打 MOTW（也无需）。
///
/// best-effort + 绝不 panic：解析失败 / cancel 失败 / MOTW 写失败只 `warn`/`debug`，不致命。连接关闭
/// （`RecvError::Closed`）→ 退出循环（backend Drop 关连接即触发）。
fn spawn_download_loop(conn: Connection) -> tokio::task::JoinHandle<()> {
    let mut begin_rx = conn.subscribe(EventDownloadWillBegin::IDENTIFIER, None);
    let mut progress_rx = conn.subscribe(EventDownloadProgress::IDENTIFIER, None);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // ① 下载发起 → 可执行 denylist 红线（命中即 cancelDownload，fail-closed/yolo 也取消）。
                ev = begin_rx.recv() => {
                    match ev {
                        Ok(ev) => {
                            let Ok(b) = serde_json::from_value::<EventDownloadWillBegin>(ev.params.clone())
                            else { continue };
                            // SD-3: Two complementary checks — filename extension denylist OR
                            // data: URL content sniffing. Either triggers the red-line cancel.
                            let filename_blocked = crate::download::reject_executable_download(&b.suggested_filename).is_err();
                            let content_blocked = crate::download::data_url_is_executable(&b.url);

                            if filename_blocked || content_blocked {
                                let reason = if content_blocked && !filename_blocked {
                                    "data: URL content sniffed as executable (magic bytes match)"
                                } else if filename_blocked && content_blocked {
                                    "executable filename extension AND data: URL content sniffed as executable"
                                } else {
                                    "executable/script filename extension"
                                };
                                tracing::warn!(
                                    target: "nomi_browser_engine::backend::cdp",
                                    guid = %b.guid, suggested = %b.suggested_filename,
                                    url_scheme = %if b.url.starts_with("data:") { "data:" } else { &b.url[..b.url.find(':').unwrap_or(0).min(10) + 1] },
                                    reason = %reason,
                                    "download blocked (red-line, denied even under yolo/companion); cancelling"
                                );
                                // 红线 enforcement：取消该下载（发到根 browser session）。失败只 warn。
                                if let Err(e) = conn
                                    .send_may_fail::<CancelDownloadParams>(
                                        ROOT_SESSION,
                                        &CancelDownloadParams::new(b.guid.clone()),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        target: "nomi_browser_engine::backend::cdp",
                                        guid = %b.guid, error = %e,
                                        "cancelDownload for a blocked download failed; the file may still land in the isolated downloads dir (block already logged)"
                                    );
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
                // ② 下载完成 → 对放行的非可执行下载打 MOTW（被取消的可执行不会到这里）。
                ev = progress_rx.recv() => {
                    match ev {
                        Ok(ev) => {
                            let Ok(p) = serde_json::from_value::<EventDownloadProgress>(ev.params.clone())
                            else { continue };
                            use chromiumoxide::cdp::browser_protocol::browser::DownloadProgressState;
                            if p.state != DownloadProgressState::Completed {
                                continue; // 只在完成时打 MOTW。
                            }
                            // 完成且有落盘路径 → 打 MOTW。无 filePath（某些平台不保证给）→ 跳过（无法定位文件）。
                            let Some(file_path) = p.file_path.as_deref() else {
                                tracing::debug!(guid = %p.guid, "download completed without filePath; skip MOTW");
                                continue;
                            };
                            let path = std::path::Path::new(file_path);
                            match crate::download::write_motw(path) {
                                Ok(()) => {
                                    tracing::debug!(file = %file_path, "download completed; MOTW applied (Windows)");
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, file = %file_path, "MOTW write failed (non-NTFS or file moved); benign");
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
    })
}

/// **E5 出口防火墙：对单个 session 挂 `Fetch.enable`**（全流量拦截）。
///
/// `EnableParams::default()`（空 `patterns`）= 拦截**所有** Request 阶段的请求（nav + XHR + fetch +
/// POST + 子资源 + beacon）。对根 browser / page / OOPIF / **service_worker** session 都调本函数——
/// **SW 必须也拦**（裁决⑪/不变量⑬：否则页面把出口请求塞进 SW 即整体绕过防火墙）。
///
/// 用 `send_may_fail`：session 可能在挂之前就 detach（target 关闭竞态），吞掉「目标已不在」类错误
/// （防火墙对一个已消失的 session 失效本就无害）。
async fn enable_fetch_on_session(conn: &Connection, session_id: &str) -> Result<(), TransportError> {
    let params = FetchEnableParams::default();
    conn.send_may_fail::<FetchEnableParams>(session_id, &params)
        .await
}

/// **E5 出口防火墙后台循环**：①订阅 `Target.attachedToTarget`（全 session 通配）→ 对每个新 session
/// （page / OOPIF / **service_worker**）挂 `Fetch.enable`；②订阅 `Fetch.requestPaused`（全 session
/// 通配）→ 对每条被拦请求经 [`crate::firewall::decide`] 判定 → 在**事件自身的 sessionId** 上发
/// `Fetch.continueRequest`（放行）/ `Fetch.failRequest{BlockedByClient}`（阻断）。
///
/// **SW 链路（裁决⑪/不变量⑬）**：本循环订阅的 `attachedToTarget` 含 service_worker（P0 保持其
/// attach、不 detach），故 SW session 同样被挂 `Fetch.enable`、其出口请求同样经本循环判定——SW 无法
/// 绕过防火墙。
///
/// **跨域 POST 门控的 enforcement 边界（E5 范围）**：[`FirewallDecision::GatePost`] 当前**放行 + 构造
/// 预览留痕**（`info` 日志记 host/size/字段名——**绝不**记字段值）；实际升 Exec 审批的人在回路路由由
/// **F1** 接线，见 `TODO(E5->F1-egress-approval)`。[`FirewallDecision::Block`]（IP 封禁）是**硬阻断**，
/// E5 即 enforce（SSRF 防护无审批语义）。
///
/// 所有错误 best-effort：单条请求判定/dispatch 失败只 `debug`/`warn`，**绝不 panic**，且**绝不**让一条
/// 请求悬挂（任何分支都对它 continue 或 fail——否则 Fetch.enable 下未应答的请求会卡住页面）。连接关闭
/// （`RecvError::Closed`）→ 退出循环（backend Drop 关连接即触发）。
fn spawn_fetch_firewall_loop(
    conn: Connection,
    config: crate::firewall::FirewallConfig,
    egress_approver: Option<Arc<dyn crate::firewall::EgressApprover>>,
    approved_domains: crate::firewall::ApprovedDomains,
    dns_resolver: Arc<dyn crate::firewall::HostResolver>,
    dns_cache: crate::firewall::DnsResolverCache,
) -> tokio::task::JoinHandle<()> {
    let mut attached_rx = conn.subscribe(EventAttachedToTarget::IDENTIFIER, None);
    let mut paused_rx = conn.subscribe(EventRequestPaused::IDENTIFIER, None);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // ① 新 session（含 SW）→ 挂 Fetch.enable。
                ev = attached_rx.recv() => {
                    match ev {
                        Ok(ev) => {
                            let Ok(att) = serde_json::from_value::<EventAttachedToTarget>(ev.params.clone())
                            else { continue };
                            let sid: String = att.session_id.clone().into();
                            let ttype = att.target_info.r#type.clone();
                            // 对所有子 target（page/iframe/service_worker/worker…）一视同仁挂 Fetch.enable。
                            // SW 在此被覆盖（裁决⑪/不变量⑬）。失败只 warn（best-effort）。
                            if let Err(e) = enable_fetch_on_session(&conn, &sid).await {
                                tracing::warn!(
                                    target: "nomi_browser_engine::backend::cdp",
                                    error = %e, session_id = %sid, target_type = %ttype,
                                    "Fetch.enable on attached session failed; egress firewall has a gap for this target"
                                );
                            } else {
                                tracing::debug!(
                                    target: "nomi_browser_engine::backend::cdp",
                                    session_id = %sid, target_type = %ttype,
                                    "Fetch.enable armed on attached session (egress firewall)"
                                );
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
                // ② 被拦请求 → 判定 → continue/fail/（D2）悬挂等审批。
                ev = paused_rx.recv() => {
                    match ev {
                        Ok(ev) => {
                            let session_id = ev.session_id.clone();
                            let Ok(paused) = serde_json::from_value::<EventRequestPaused>(ev.params.clone())
                            else {
                                // 解析失败：仍尽力放行该请求（无法判定但不能让它悬挂）。但我们没有
                                // request_id 就无法 continue——解析失败时 request_id 也拿不到，只能跳过
                                // （CDP 会因超时自己处理；这是极端边角）。
                                continue;
                            };
                            handle_paused_request(
                                &conn,
                                &config,
                                egress_approver.as_ref(),
                                &approved_domains,
                                &session_id,
                                paused,
                                dns_resolver.as_ref(),
                                &dns_cache,
                            )
                            .await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        }
    })
}

/// 处理一条 `Fetch.requestPaused`：抽取判定所需输入 → [`crate::firewall::decide`] → dispatch
/// continue/fail/（D2）**悬挂等审批**。**绝不**让请求**无条件**悬挂（Allow/Block 立即 continue/fail；
/// GatePost 走 D2 悬挂机制，仍有界——超时即 fail-closed，绝不永久挂起）。
///
/// **P3-D2（裁决④/决策3）**：[`crate::firewall::FirewallDecision::GatePost`] 不再 detect-but-continue
/// （P2 泄漏窗口），而是：
/// 1. 先查 `approved_domains`（决策3 always_allow 记住域）——目标 eTLD+1 已被本会话批准 → **直接
///    continue**（不再悬挂审批）；
/// 2. 否则**悬挂**该请求（保留 `request_id`，**不**立即 continue/fail）+ `tokio::spawn` 一个 detached
///    任务（事件循环立即回到 `select!` 继续 pump，**绝不**在此同步阻塞）；该任务 `await`
///    [`crate::firewall::EgressApprover`]（带 [`crate::firewall::EGRESS_APPROVAL_TIMEOUT`] 超时）取裁决 →
///    批准 `continueRequest`（可选记住域）/ 拒绝/超时/**无审批通道** `failRequest`（**fail-closed**，
///    闭合 P2 泄漏窗口）。
// egress firewall 上下文参数较多（config/approver/approved_domains/resolver/cache）；SD-5 接入真实
// egress approver 时再收拢成一个 EgressContext 结构体（届时参数更多，结构体更划算），此处先 allow。
#[allow(clippy::too_many_arguments)]
async fn handle_paused_request(
    conn: &Connection,
    config: &crate::firewall::FirewallConfig,
    egress_approver: Option<&Arc<dyn crate::firewall::EgressApprover>>,
    approved_domains: &crate::firewall::ApprovedDomains,
    session_id: &str,
    paused: EventRequestPaused,
    dns_resolver: &dyn crate::firewall::HostResolver,
    dns_cache: &crate::firewall::DnsResolverCache,
) {
    let request_id = paused.request_id.clone();
    let url = paused.request.url.clone();
    let method = paused.request.method.clone();

    // 当前页 origin：优先请求自带的 `Origin` 头（浏览器对跨域写请求会带），退而求其次 `Referer`。
    // 这就是「发起请求的文档」的 origin——跨域判定的左侧。两者都没有（同源导航 / 顶层 nav）→ 用目标
    // URL 自身（同 host 比较恒非跨域，等价于「不门控顶层同站导航」，合理）。
    let headers = &paused.request.headers;
    let current_origin = header_value(headers, "Origin")
        .or_else(|| header_value(headers, "origin"))
        .or_else(|| header_value(headers, "Referer"))
        .or_else(|| header_value(headers, "referer"))
        .unwrap_or_else(|| url.clone());
    let content_type = header_value(headers, "Content-Type")
        .or_else(|| header_value(headers, "content-type"));

    // body：从 post_data_entries 解 base64（Fetch.requestPaused 在 Request 阶段通常带 entries）。
    let body = decode_post_data_entries(&paused);
    let has_post_data = paused.request.has_post_data.unwrap_or(false) || body.is_some();

    // 目标 host 若**本身是 IP 字面量**（最危险的 SSRF 形态：直接拿内网/元数据 IP 当 URL）→ 同步判 IP
    // 封禁，无需 DNS。域名 host 的异步 DNS→IP 路径见 TODO（E5 同步覆盖 IP 字面量这一主面）。
    let target_host = nomifun_secret::host_of(&url);
    let resolved_ip = target_host
        .as_deref()
        .and_then(crate::firewall::ip_literal_of_host);

    // 是否顶层 Document 导航（resourceType==Document）。域名 allowlist 出口门控对顶层导航豁免——allowlist
    // 是出口/数据外泄控制（限制跨域子请求把数据发往哪），不是导航监狱；agent 导航到一个 URL 是意图行为，
    // 不该因「注册 secret → allowlist 非空」被关进白名单域。**仅豁免 allow 白名单**：IP 封禁（SSRF/元数据）
    // 对导航仍生效、deny 黑名单对导航仍硬拦、跨域 POST 门控不受影响（见 firewall::decide / domain_policy）。
    let is_top_level_navigation = paused.resource_type == ResourceType::Document;

    // ─── SD-1: DNS→IP SSRF guard（egress 子资源）────────────────────────────────
    // 对 **egress 子资源请求**（非顶层 Document 导航），当 `block_private_ips` 开且目标 host 是域名
    // （非 IP 字面量）→ 异步 DNS 解析 + 检查所有 resolved IPs 是否命中 is_blocked_ip。
    // ANY 命中 / 解析失败 → fail-closed (Block)。**仅 egress-only**（不拦顶层导航——与 allowlist 豁免同理，
    // 但 IP 字面量的同步判定对 top-nav 仍生效，见 decide 的 IP 封禁档）。
    if config.block_private_ips && !is_top_level_navigation && resolved_ip.is_none() {
        // resolved_ip==None 意味 host 不是 IP 字面量（是域名）→ 需 DNS 解析。
        if let Some(host) = target_host.as_deref()
            && crate::firewall::check_dns_ssrf(host, dns_resolver, dns_cache).await
        {
            tracing::warn!(
                target: "nomi_browser_engine::backend::cdp",
                url = %url, host = %host,
                "egress firewall BLOCKED request: domain resolves to private/metadata IP (DNS→IP SSRF guard)"
            );
            fetch_fail(conn, session_id, request_id).await;
            return;
        }
    }

    let decision = crate::firewall::decide(
        config,
        &crate::firewall::RequestInfo {
            resolved_ip,
            method: &method,
            has_post_data,
            body: body.as_deref(),
            content_type: content_type.as_deref(),
            current_origin: &current_origin,
            target_url: &url,
            is_top_level_navigation,
        },
    );

    match decision {
        crate::firewall::FirewallDecision::Allow => {
            fetch_continue(conn, session_id, request_id).await;
        }
        crate::firewall::FirewallDecision::Block { reason } => {
            // 硬阻断（IP 封禁，SSRF 防护）。failRequest{BlockedByClient}。
            tracing::warn!(
                target: "nomi_browser_engine::backend::cdp",
                url = %url, reason = %reason,
                "egress firewall BLOCKED request (failRequest)"
            );
            fetch_fail(conn, session_id, request_id).await;
        }
        crate::firewall::FirewallDecision::GatePost { preview } => {
            // P3-D2（裁决④/决策3）：闭合 P2 跨域 POST 泄漏窗口——不再 detect-but-continue。
            //
            // ① 决策3 always_allow：目标 eTLD+1 已被本会话批准（用户此前审批时选「记住此域」）→
            //    直接放行（不再悬挂审批）。同域后续提交不再反复弹。
            if approved_domains.is_approved(&url) {
                tracing::debug!(
                    target: "nomi_browser_engine::backend::cdp",
                    target_host = %preview.host,
                    "egress firewall: gated request to an already-approved domain (always_allow) — continuing"
                );
                fetch_continue(conn, session_id, request_id).await;
                return;
            }

            // ② 悬挂该请求等人在回路裁决。**绝不**在此 CDP 事件 handler 里同步阻塞（会卡死整个
            //    防火墙事件循环——所有 session 的 requestPaused/attachedToTarget 都经它）。故把
            //    request_id 保留（不 continue/不 fail），spawn 一个 detached 任务去 await 审批 → 据裁决
            //    continue/fail。审批通道未接入（egress_approver=None）/ 超时 / 拒绝 → **fail-closed**
            //    （failRequest）。预览只 host/size/字段名（绝不含值，复用 E5 build_post_preview）。
            tracing::info!(
                target: "nomi_browser_engine::backend::cdp",
                target_host = %preview.host, body_size = preview.size,
                field_names = ?preview.field_names, // 仅字段名（绝不含值）
                "egress firewall gated cross-origin POST / off-allowlist egress — suspending for out-of-band approval (fail-closed on timeout/no-channel)"
            );

            // 句柄 + 上下文克隆进 detached 任务（Connection 内部 Arc，克隆廉价；request_id/session_id/url
            // owned；approver Arc 克隆；approved_domains 克隆共享同一份 Arc<Mutex<…>>）。
            let conn = conn.clone();
            let session_id = session_id.to_string();
            let url = url.clone();
            let approver = egress_approver.cloned();
            let approved_domains = approved_domains.clone();
            tokio::spawn(async move {
                let verdict = match approver {
                    // 有审批通道：await 裁决（带超时——绝不无限悬挂）。
                    Some(a) => {
                        match tokio::time::timeout(
                            crate::firewall::EGRESS_APPROVAL_TIMEOUT,
                            a.approve_egress(&preview),
                        )
                        .await
                        {
                            Ok(v) => v,
                            Err(_elapsed) => {
                                tracing::warn!(
                                    target: "nomi_browser_engine::backend::cdp",
                                    target_host = %preview.host,
                                    "egress approval timed out — failing closed (rejecting the gated request)"
                                );
                                crate::firewall::EgressVerdict::Fail
                            }
                        }
                    }
                    // 无审批通道接入 → fail-closed（闭合泄漏窗口；拒绝跨域 POST 比放行安全）。
                    None => {
                        tracing::warn!(
                            target: "nomi_browser_engine::backend::cdp",
                            target_host = %preview.host,
                            "egress firewall gated a request but no approval channel is wired — failing closed"
                        );
                        crate::firewall::EgressVerdict::Fail
                    }
                };
                match verdict {
                    v if v.is_continue() => {
                        // 批准放行。决策3 always_allow：仅当裁决是 ContinueAndRemember 才把目标域记进
                        // 本会话已批准集合（同域后续不再问）；一次性 Continue 不记。IP 字面量 / 无 eTLD+1
                        // 的目标不会被记入（registrable_domain_for_trust fail-closed）。
                        if v.remembers_domain() {
                            approved_domains.record(&url);
                        }
                        fetch_continue(&conn, &session_id, request_id).await;
                    }
                    _ => {
                        fetch_fail(&conn, &session_id, request_id).await;
                    }
                }
            });
        }
    }
}

/// 从 `Headers`（`serde_json::Value` object）取某头的值（精确 key 匹配；调用方自己试大小写变体）。
fn header_value(
    headers: &chromiumoxide::cdp::browser_protocol::network::Headers,
    key: &str,
) -> Option<String> {
    headers
        .inner()
        .as_object()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 从 `Fetch.requestPaused` 的 `request.post_data_entries`（base64 `bytes`）解出 body 字节。
/// 无 entries / 解码失败 → `None`（判定层据 `has_post_data` 仍可判「有 body 但内容不可见」）。
fn decode_post_data_entries(paused: &EventRequestPaused) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let entries = paused.request.post_data_entries.as_ref()?;
    let mut out: Vec<u8> = Vec::new();
    for e in entries {
        if let Some(bin) = &e.bytes {
            let s: &str = bin.as_ref();
            // CDP post data entry bytes 是 base64 编码。解码失败的 entry 跳过（best-effort）。
            if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(s) {
                out.extend_from_slice(&decoded);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 放行被拦请求（`Fetch.continueRequest`）。best-effort：失败只 debug（请求会因 CDP 超时自处理）。
async fn fetch_continue(
    conn: &Connection,
    session_id: &str,
    request_id: chromiumoxide::cdp::browser_protocol::fetch::RequestId,
) {
    let params = ContinueRequestParams::new(request_id);
    if let Err(e) = conn
        .send_may_fail::<ContinueRequestParams>(session_id, &params)
        .await
    {
        tracing::debug!(
            target: "nomi_browser_engine::backend::cdp",
            error = %e, "Fetch.continueRequest failed (benign; request may have already resolved)"
        );
    }
}

/// 阻断被拦请求（`Fetch.failRequest{BlockedByClient}`）。best-effort：失败只 debug。
async fn fetch_fail(
    conn: &Connection,
    session_id: &str,
    request_id: chromiumoxide::cdp::browser_protocol::fetch::RequestId,
) {
    let params = FailRequestParams::new(request_id, NetworkErrorReason::BlockedByClient);
    if let Err(e) = conn
        .send_may_fail::<FailRequestParams>(session_id, &params)
        .await
    {
        tracing::debug!(
            target: "nomi_browser_engine::backend::cdp",
            error = %e, "Fetch.failRequest failed (benign; request may have already resolved)"
        );
    }
}


///
/// `force_headless = !display_available() || (display 可用但配置要 headless)`。本函数把
/// `display_available()` 与 `config.headful` 合成 `force_headless` 传给 launch。
///
/// `download_dir`（E4）：per-pet 隔离下载目录的绝对路径（[`crate::download::ensure_download_dir`]
/// 的产物）。`Some` → 启动时挂 `Browser.setDownloadBehavior(allowAndName, download_dir)` 沙箱 +
/// 起下载事件循环（落盘后打 Win MOTW）。`None` → 不主动配置下载行为（chrome 默认 deny / 用户
/// 自定，仅用于不关心下载的纯冒烟）。
// build_backend / from_launched 的构造参数随安全配置增长（download/workspace/evaluate_full_power/
// evaluate_persistent_login/firewall/egress_approver/storage_state）；待后续 cleanup 收拢成
// 一个 EngineRuntimeParams 结构体（已第二次触 too_many_arguments），此处先 allow。
#[allow(clippy::too_many_arguments)]
pub async fn build_backend(
    config: &LaunchConfig,
    download_dir: Option<String>,
    workspace_dir: Option<PathBuf>,
    evaluate_full_power: bool,
    evaluate_persistent_login: bool,
    firewall: crate::firewall::FirewallConfig,
    egress_approver: Option<Arc<dyn crate::firewall::EgressApprover>>,
    storage_state: Option<serde_json::Value>,
    known_secret_values: crate::KnownSecretValues,
) -> Result<CdpBackend, BrowserError> {
    let display = crate::display::display_available();
    // 无显示器 → 强制 headless；有显示器 → 听 config.headful（false 即仍 headless）。
    let force_headless = !display || !config.headful;

    let launched = launch_chrome(config, force_headless).await?;
    // headful 仅当「有显示且配置要」。
    let headful = display && config.headful;
    CdpBackend::from_launched(
        launched,
        headful,
        display,
        download_dir,
        workspace_dir,
        evaluate_full_power,
        evaluate_persistent_login,
        firewall,
        egress_approver,
        storage_state,
        known_secret_values,
        // 生产走默认 TokioResolver（真实 DNS）。可注入 resolver 仅为测试隔离而存在。
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── B6 detach/crash 事件源接线：事件 params 形状判定（[纯逻辑]，喂构造 Value，无浏览器）──

    #[test]
    fn event_session_matches_by_session_id() {
        // Target.detachedFromTarget / targetCrashed 的 params.sessionId 命中本 page session → true。
        let p = serde_json::json!({"sessionId": "PAGE_SID", "targetId": "T1"});
        assert!(event_session_matches(&p, "PAGE_SID"));
        // 不同 session（其它 target 没了）→ false（不误 abort 本动作）。
        assert!(!event_session_matches(&p, "OTHER_SID"));
        // 缺 sessionId 字段 → false（保守不 abort）。
        let p2 = serde_json::json!({"targetId": "T1"});
        assert!(!event_session_matches(&p2, "PAGE_SID"));
        // sessionId 非字符串（坏形状）→ false。
        let p3 = serde_json::json!({"sessionId": 7});
        assert!(!event_session_matches(&p3, "PAGE_SID"));
    }

    #[test]
    fn event_frame_matches_by_frame_id() {
        // Page.frameDetached 的 params.frameId 命中动作所在帧 → true。
        let p = serde_json::json!({"frameId": "FRAME_A", "reason": "remove"});
        assert!(event_frame_matches(&p, "FRAME_A"));
        // 不同帧（无关子帧 detach）→ false（不误 abort 本动作）。
        assert!(!event_frame_matches(&p, "FRAME_B"));
        // 缺 frameId → false。
        let p2 = serde_json::json!({"reason": "remove"});
        assert!(!event_frame_matches(&p2, "FRAME_A"));
        // frameId 非字符串 → false。
        let p3 = serde_json::json!({"frameId": null});
        assert!(!event_frame_matches(&p3, "FRAME_A"));
    }

    #[test]
    fn transport_timeout_maps_to_nav_failed() {
        let e = map_transport_err(TransportError::Timeout);
        assert!(matches!(e, BrowserError::NavFailed { .. }));
    }

    #[test]
    fn transport_closed_maps_to_session_lost_unrecoverable() {
        assert!(matches!(
            map_transport_err(TransportError::Closed),
            BrowserError::SessionLost { recoverable: false }
        ));
        assert!(matches!(
            map_transport_err(TransportError::SessionClosed),
            BrowserError::SessionLost { recoverable: false }
        ));
    }

    #[test]
    fn transport_crashed_maps_to_session_lost_recoverable() {
        assert!(matches!(
            map_transport_err(TransportError::SessionCrashed),
            BrowserError::SessionLost { recoverable: true }
        ));
    }

    #[test]
    fn transport_cdp_error_maps_to_other_with_code() {
        let e = map_transport_err(TransportError::Cdp {
            code: -32000,
            message: "Cannot find context".into(),
        });
        match e {
            BrowserError::Other(msg) => {
                assert!(msg.contains("-32000"), "msg: {msg}");
                assert!(msg.contains("Cannot find context"), "msg: {msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn transport_protocol_maps_to_other() {
        let e = map_transport_err(TransportError::Protocol("bad".into()));
        assert!(matches!(e, BrowserError::Other(_)));
    }

    #[test]
    fn map_inject_err_classifies_all_variants() {
        // JsException → Other（保留原文）。
        match map_inject_err(InjectError::JsException("boom".into())) {
            BrowserError::Other(msg) => assert_eq!(msg, "boom"),
            other => panic!("expected Other, got {other:?}"),
        }
        // Protocol → Other（保留原文）。
        match map_inject_err(InjectError::Protocol("weird shape".into())) {
            BrowserError::Other(msg) => assert_eq!(msg, "weird shape"),
            other => panic!("expected Other, got {other:?}"),
        }
        // ContextNotReady → NavFailed{kind:"context"}（与 frame_id 无关，恒定 kind）。
        match map_inject_err(InjectError::ContextNotReady {
            frame_id: "F0".into(),
        }) {
            BrowserError::NavFailed { kind } => assert_eq!(kind, "context"),
            other => panic!("expected NavFailed, got {other:?}"),
        }
        // Transport(Timeout) 经 map_transport_err 复用 → NavFailed。
        assert!(matches!(
            map_inject_err(InjectError::Transport(TransportError::Timeout)),
            BrowserError::NavFailed { .. }
        ));
        // Transport(Closed) 复用 → SessionLost{recoverable:false}（确认确实走 map_transport_err 全语义）。
        assert!(matches!(
            map_inject_err(InjectError::Transport(TransportError::Closed)),
            BrowserError::SessionLost { recoverable: false }
        ));
    }

    #[test]
    fn base64_roundtrip_png_magic() {
        // "PNG" 的 base64 是 "UE5H"；解码应得回 b"PNG"。
        assert_eq!(decode_base64("UE5H"), Some(b"PNG".to_vec()));
    }

    #[test]
    fn base64_handles_padding() {
        // "hi" → "aGk="（含填充）。CDP 截图 data 是干净标准 base64，无需容忍内嵌空白。
        assert_eq!(decode_base64("aGk="), Some(b"hi".to_vec()));
    }

    #[test]
    fn base64_rejects_invalid_char() {
        assert!(decode_base64("not base64 !@#").is_none());
    }

    #[test]
    fn base64_real_png_header() {
        // 真 PNG 文件头 8 字节: 137 80 78 71 13 10 26 10 → base64 "iVBORw0KGgo="。
        let decoded = decode_base64("iVBORw0KGgo=").unwrap();
        assert_eq!(&decoded[0..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        // 注意：&decoded[1..4] == b"PNG"（冒烟断言用的就是这个切片）。
        assert_eq!(&decoded[1..4], b"PNG");
    }

    // ── observe 纯逻辑 helper（无浏览器）：ref/role/name 解析 + 递归缝合 + depth 粗判 ──

    #[test]
    fn parse_ref_token_extracts_ref_only() {
        assert_eq!(
            parse_ref_token(r#"  - button "Submit order" [ref=f0e1]"#).as_deref(),
            Some("f0e1")
        );
        // [cursor=pointer] 后缀不混入 ref。
        assert_eq!(
            parse_ref_token("  - link \"X\" [ref=f2e9] [cursor=pointer]").as_deref(),
            Some("f2e9")
        );
        // 无 ref 行 → None。
        assert_eq!(parse_ref_token("  - text: hello"), None);
    }

    #[test]
    fn parse_seq_from_ref_extracts_frame_seq() {
        assert_eq!(parse_seq_from_ref("f0e1"), Some(0));
        assert_eq!(parse_seq_from_ref("f12e345"), Some(12));
        assert_eq!(parse_seq_from_ref("bogus"), None);
        assert_eq!(parse_seq_from_ref("fXe1"), None);
    }

    #[test]
    fn parse_role_name_splits_role_and_quoted_name() {
        assert_eq!(
            parse_role_name(r#"  - button "Submit order" [ref=f0e1]"#),
            ("button".to_string(), "Submit order".to_string())
        );
        // 无 name（如 iframe / generic）→ name 空。
        assert_eq!(
            parse_role_name("  - iframe [ref=f0e5]"),
            ("iframe".to_string(), String::new())
        );
        // 带属性标记的 role 仍只取 role token。
        assert_eq!(
            parse_role_name(r#"- checkbox "Remember me" [checked] [ref=f0e3]"#),
            ("checkbox".to_string(), "Remember me".to_string())
        );
    }

    #[test]
    fn extract_quoted_respects_escapes() {
        assert_eq!(extract_quoted(r#"a "he said \"hi\"" b"#).as_deref(), Some("he said \"hi\""));
        assert_eq!(extract_quoted("no quotes here"), None);
    }

    #[test]
    fn frame_hit_depth_limit_detects_deep_indent() {
        let shallow = FrameSnapshot {
            full: "- button \"X\" [ref=f0e1]".into(),
            incremental: None,
            iframe_refs: vec![],
            iframe_depths: std::collections::HashMap::new(),
        };
        assert!(!frame_hit_depth_limit(&shallow, 12));
        // 一行缩进达 24 空格 = depth 12 层 → 触顶。
        let deep = FrameSnapshot {
            full: format!("{}- text: deep", " ".repeat(24)),
            incremental: None,
            iframe_refs: vec![],
            iframe_depths: std::collections::HashMap::new(),
        };
        assert!(frame_hit_depth_limit(&deep, 12));
        // max_depth=0 = 不封顶 → 恒 false。
        assert!(!frame_hit_depth_limit(&deep, 0));
    }

    #[test]
    fn render_frame_recursive_stitches_nested_frames() {
        // 三帧树：f0(主) → f1(子, 经 ref f0e5) → f2(孙, 经 ref f1e3)。
        let frames = vec![
            ObservedFrame {
                seq: 0,
                frame_id: "MAIN".into(),
                session_id: "S".into(),
                snapshot: FrameSnapshot {
                    full: "- generic:\n  - iframe [ref=f0e5]".into(),
                    incremental: None,
                    iframe_refs: vec!["f0e5".into()],
                    iframe_depths: std::collections::HashMap::from([("f0e5".to_string(), 1u32)]),
                },
            },
            ObservedFrame {
                seq: 1,
                frame_id: "CHILD".into(),
                session_id: "S".into(),
                snapshot: FrameSnapshot {
                    full: "- iframe [ref=f1e3]".into(),
                    incremental: None,
                    iframe_refs: vec!["f1e3".into()],
                    iframe_depths: std::collections::HashMap::from([("f1e3".to_string(), 0u32)]),
                },
            },
            ObservedFrame {
                seq: 2,
                frame_id: "GRAND".into(),
                session_id: "S".into(),
                snapshot: FrameSnapshot {
                    full: "- link \"Deep\" [ref=f2e1]".into(),
                    incremental: None,
                    iframe_refs: vec![],
                    iframe_depths: std::collections::HashMap::new(),
                },
            },
        ];
        let parent_of = HashMap::from([
            ("CHILD".to_string(), ("MAIN".to_string(), "f0e5".to_string())),
            ("GRAND".to_string(), ("CHILD".to_string(), "f1e3".to_string())),
        ]);
        let out = render_frame_recursive(&frames, 0, &parent_of);
        // 主帧 iframe 行内联子帧，子帧 iframe 行再内联孙帧。
        assert!(out.contains("- iframe [ref=f0e5]:"), "out:\n{out}");
        assert!(out.contains("f1e3]:"), "child iframe should be opened:\n{out}");
        assert!(out.contains("Deep"), "grandchild content missing:\n{out}");
        // 孙内容缩进应比子内容更深。
        let deep_line = out.lines().find(|l| l.contains("Deep")).unwrap();
        let child_line = out.lines().find(|l| l.contains("f1e3]")).unwrap();
        let deep_indent = deep_line.len() - deep_line.trim_start().len();
        let child_indent = child_line.len() - child_line.trim_start().len();
        assert!(deep_indent > child_indent, "grandchild not deeper than child");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // F-actions：download 落盘探测 + save_as_pdf 输出路径（[纯逻辑] + 真 FS temp dir，无浏览器）。
    // ═══════════════════════════════════════════════════════════════════════

    /// 给本测建一个唯一临时目录（按测试名 + pid 去歧义，避免并行测试互踩）。
    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nomifun-facts-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn list_dir_files_collects_filenames_and_skips_subdirs() {
        let dir = unique_tmp_dir("listdir");
        // 清场（防上次残留干扰）。
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.pdf"), b"y").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let set = list_dir_files(dir.to_str().unwrap());
        assert!(set.contains("a.txt"));
        assert!(set.contains("b.pdf"));
        assert!(!set.contains("sub"), "subdirectories must not be listed");
        // 不存在的目录 → 空集（best-effort，不 panic）。
        assert!(list_dir_files("/no/such/dir/zzz-nonexistent").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_completed_download_finds_new_nonempty_nontemp_file() {
        let dir = unique_tmp_dir("newdl");
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        let dir_s = dir.to_str().unwrap();
        // 基线：触发前已有 old.txt（不应被当作本次下载）。
        std::fs::write(dir.join("old.txt"), b"old").unwrap();
        let before = list_dir_files(dir_s);
        assert!(before.contains("old.txt"));

        // 新增一个非空、非临时的文件 → 命中。
        std::fs::write(dir.join("report.pdf"), b"%PDF-1.4 some bytes").unwrap();
        let found = newest_completed_download(dir_s, &before);
        assert!(found.is_some(), "a new non-empty file must be detected");
        let (name, size) = found.unwrap();
        assert_eq!(name, "report.pdf");
        assert!(size > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_completed_download_skips_crdownload_empty_and_preexisting() {
        let dir = unique_tmp_dir("skipdl");
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let _ = std::fs::remove_file(e.path());
        }
        let dir_s = dir.to_str().unwrap();
        let before = list_dir_files(dir_s); // 空基线

        // chrome 下载中间态（仍在传）→ 不算完成，跳过。
        std::fs::write(dir.join("inflight.crdownload"), b"partial").unwrap();
        // 0 字节（刚创建占位）→ 跳过。
        std::fs::write(dir.join("placeholder.bin"), b"").unwrap();
        assert!(
            newest_completed_download(dir_s, &before).is_none(),
            "only .crdownload + empty files present → no completed download"
        );

        // 触发前已存在的文件（即便非空）→ 不算本次（在 before 集里）。
        let before2 = {
            std::fs::write(dir.join("preexisting.pdf"), b"old-but-nonempty").unwrap();
            list_dir_files(dir_s)
        };
        assert!(
            newest_completed_download(dir_s, &before2).is_none(),
            "a file already in the baseline must not be counted as this download"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pdf_output_path_is_under_downloads_with_pdf_extension() {
        let path = pdf_output_path("/some/companion/workspace/downloads");
        assert!(path.starts_with("/some/companion/workspace/downloads"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("page-"), "filename: {name}");
        assert!(name.ends_with(".pdf"), "filename: {name}");
    }
}

/// **op_mutex 序列化纪律（纯逻辑钉死）**：镜像 [`CdpBackend::op_mutex`] 的 acquire 模式（一把
/// `AsyncMutex<()>` 跨整个操作体持有），无需启动 Chrome 即证明两操作不交错。真 Chrome 的 observe⊥act
/// 交错冒烟见 `tests/op_mutex_concurrency.rs`（`#[ignore]`，需 `NOMIFUN_CHROME_BINARY`）。
#[cfg(test)]
mod op_mutex_tests {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    #[tokio::test]
    async fn op_mutex_serializes_two_operations() {
        let op_mutex = Arc::new(AsyncMutex::new(()));
        let order = Arc::new(AsyncMutex::new(Vec::<&'static str>::new()));

        // 操作 A：抢到 op_mutex 后跨 await 持有一段时间。
        let (m1, o1) = (op_mutex.clone(), order.clone());
        let a = tokio::spawn(async move {
            let _g = m1.lock().await;
            o1.lock().await.push("a-start");
            tokio::time::sleep(Duration::from_millis(20)).await;
            o1.lock().await.push("a-end");
        });
        // 让 A 先抢到锁。
        tokio::time::sleep(Duration::from_millis(5)).await;
        // 操作 B：必须等 A 整段结束才能拿到锁。
        let (m2, o2) = (op_mutex.clone(), order.clone());
        let b = tokio::spawn(async move {
            let _g = m2.lock().await;
            o2.lock().await.push("b-start");
            o2.lock().await.push("b-end");
        });

        a.await.unwrap();
        b.await.unwrap();
        // B 不得在 A 结束前开始 → observe⊥act 不交错。
        let seen = order.lock().await.clone();
        assert_eq!(
            seen,
            vec!["a-start", "a-end", "b-start", "b-end"],
            "op_mutex 必须串行（无交错）: {seen:?}"
        );
    }
}

