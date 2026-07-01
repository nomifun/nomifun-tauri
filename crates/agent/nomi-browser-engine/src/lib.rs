//! nomi-browser-engine —— 进程内自研 Rust CDP 浏览器引擎（Chromium-only，不打包
//! Playwright/Node）。
//!
//! 端到端入口是 [`create_engine`]：resolve chrome（打包优先/下载兜底）→ 托管启动
//! （随机端口 + 专属 user-data-dir + headless 自动降级）→ 自建 transport connect →
//! flatten setAutoAttach → 取一个 page session → 返回实现 [`engine::BrowserEngine`] 的
//! [`backend::CdpBackend`]（方案 A：自建 transport 发裸 CDP 命令，**不**用 chromiumoxide
//! 高层 `Browser`/`Page`）。

pub mod acquire;
pub mod actionability;
pub mod actions;
pub mod aria_ref;
pub mod backend;
pub mod debug_capture;
pub mod display;
pub mod download;
pub mod engine;
pub mod errmap;
pub mod evaluate;
pub mod firewall;
pub mod injected;
pub mod input;
pub mod launch;
pub mod nav;
pub mod observe;
pub mod profile;
pub mod progress;
pub mod redact;
pub mod selector;
pub mod session;
pub mod storage_state;
pub mod switches;
pub mod tabs;
pub mod transport;
pub mod vault;

/// 公共 API 再导出：调用方用 `nomi_browser_engine::{BrowserEngine, Capabilities, …}`，
/// 无需知晓子模块布局（返回类型 `Arc<dyn BrowserEngine>` 的 trait 与配套类型即此公开面）。
pub use engine::{
    BrowserEngine, BrowserError, Capabilities, CssRect, DetachKind, ElementEntry, LoadState,
    NavPhase, NavResult, Observation, ObserveOpts, SnapshotGen,
};
pub use actions::{
    ActResult, ActSpec, Effect, ScrollDir, ScrollTarget, TypeInput, WaitCondition,
};
pub use debug_capture::DebugSnapshot;
/// **P3-D2：出口审批通道公开面（被门控请求悬挂等裁决的接缝 + 裁决枚举 + always_allow 集合）。
/// 下游（facade/网关）接 GW2 审批通道时实现 [`EgressApprover`]，经 [`EngineConfig::egress_approver`]
/// 注入。也可经 `nomi_browser_engine::firewall::*` 全路径访问。
pub use firewall::{ApprovedDomains, EgressApprover, EgressVerdict, FirewallConfig, PostPreview};
/// **P3-W4b/W4c：storage_state（cookie + localStorage）公开面**（DESIGN §17 / 裁决⑥）。默认 browser context 登录态
/// 的 cookie（W4b）+ localStorage（W4c，origin-bound）捕获/恢复结构 + CDP 转换。下游（W4d vault / 网关）
/// 用 [`StorageState`] 在 `EngineConfig.storage_state` 与磁盘 vault 间往返。IndexedDB 是 best-effort/TODO。
pub use storage_state::{
    IdbDatabase, IdbStore, IndexedDbDump, LocalStorageItem, OriginStorage, StorageState,
    StorageStateCookie, IDB_BASE64_SENTINEL, decode_binary_sentinel, encode_binary_sentinel,
};
/// **P3-W4d：storage_state 持久化 vault（加密）公开面**（DESIGN §17 / 决策1，吸收原 P6）。
/// 把 [`StorageState`] 登录态 AES-256-GCM 加密落盘到共享 vault（[`vault::save_storage_state`]）/
/// 跨会话解密读回（[`vault::load_storage_state`]，坏 vault 优雅退回 `None`），喂 `EngineConfig.storage_state`
/// 启动注入实现**持久登录**。crypto 复用 `nomifun_common`（裁决⑦不另起第二套栈）。
pub use vault::{
    SHARED_STORAGE_STATE_DIR, VaultError, load_storage_state, save_storage_state,
    shared_storage_state_path, storage_state_path,
};
/// **浏览器来源**（「浏览器模式」的来源维度，与 headless 正交）：托管 CfT vs 系统 Chrome/Edge。
/// 见 [`acquire::ChromeSource`]。经 [`EngineConfig::chrome_source`] 注入 → `create_engine` 解析二进制。
pub use acquire::ChromeSource;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::backend::cdp::build_backend;

/// Session-scoped registry of the agent's own resolved secret plaintext values (from the
/// facade's `secret:NAME` → vault resolution). Used **solely** for deterministic exact-blackout
/// redaction in debug serializers: any debug output containing one of these values has it
/// replaced with `[KNOWN_SECRET_REDACTED]` BEFORE heuristic passes run.
///
/// **Security invariants:**
/// - Contains ONLY values that the agent already handles in plaintext during the current session
///   (they are injected via `Input.insertText` anyway), so this is NOT a new exposure category.
/// - In-memory only, session-scoped (dropped when the engine/facade is dropped).
/// - Used exclusively for read-side redaction (never written to disk, logs, or network).
/// - Only values with `len >= 4` are inserted (avoid over-matching trivial values).
///
/// The facade (BrowserTool) owns the canonical Arc and populates it on each successful
/// `secret:NAME` resolution; the engine holds a clone and reads it during serialization.
pub type KnownSecretValues = Arc<std::sync::Mutex<HashSet<String>>>;
use crate::launch::LaunchConfig;

/// 创建引擎的配置。`Default` 给出合理本机默认（临时数据目录、无打包目录、headless）。
#[derive(Clone)]
pub struct EngineConfig {
    /// 应用数据目录：下载兜底的 chrome 落点 + **专属** user-data-dir 的父目录。
    /// 默认 `std::env::temp_dir()/nomifun-browser-data`。
    pub data_dir: PathBuf,
    /// 打包资源目录（Tauri resource dir，build 期固化的 chrome）。默认 None。
    pub bundled_dir: Option<PathBuf>,
    /// 是否希望带可见窗口。注意：无显示器时本标志被忽略，强制 headless。默认 false。
    pub headful: bool,
    /// **浏览器来源**（[`ChromeSource`]，与 `headful` 正交）：`Managed`（默认）= 内置/下载的
    /// Chrome for Testing；`System`（「我的浏览器」）= 系统已装 Chrome/Edge 本体优先（未探到
    /// 回退 Managed）。上层从 `client_preferences` `agent.browserUse.source` 解析后灌入
    /// （facade `BrowserTool` 由 `BrowserConfig.source` 解析）。**两种来源都配专属
    /// user-data-dir 起独立托管实例**——红线不变：绝不碰用户真实 profile。
    pub chrome_source: ChromeSource,
    /// **E4 下载沙箱落点**：per-pet 隔离 workspace 目录（companion.rs 的
    /// `{companion_id}/workspace`）。下载经 `Browser.setDownloadBehavior(allowAndName)` 落进
    /// 它的 `downloads/` 子目录（[`download::download_dir`]）——**绝不**落用户真实 Downloads。
    /// `None`（无 per-pet 上下文，如纯引擎冒烟）→ 兜底落 `<data_dir>/downloads`（仍是我们自己的
    /// 隔离目录，非用户 Downloads）。
    pub workspace_dir: Option<PathBuf>,
    /// **E3/F1-sec：evaluate「全权模式」LIVE 值**（裁决⑨）。用户在 System Settings 显式 opt-in 的
    /// 全权开关（`client_preferences` 形如 `agent.browserUse.fullPower`，由上层经 `read_bool_pref`
    /// 范式 LIVE 读后灌进来）。`false`（默认 / 未 opt-in）→ evaluate 默认 OFF（`Unsupported`）。
    /// `true` → evaluate 放行（仍受「与持久登录互斥」约束，见 [`crate::evaluate`]）。**绝不看
    /// session_mode**——yolo/companion 无从豁免（不变量⑧）。本值在引擎构造期灌入
    /// [`crate::evaluate::EvaluateGate::full_power`]。
    pub evaluate_full_power: bool,
    /// **SD-6：持久登录 LIVE 值**（DESIGN §16/§27 互斥约束）。上层从 `client_preferences`
    /// 经 `read_bool_pref` 范式 LIVE 读后灌进来（key `agent.browserUse.persistentLogin`，
    /// host_default=true — 产品默认 ON）。`true` → 与全权互斥（[`crate::evaluate::check_full_power_eligible`]
    /// 两者皆 true → `Blocked`）。**代码级 Default = `false`**（default-deny 基线，与 evaluate_full_power
    /// 同范式；产品 ON 仅由 factory 的 host_default=true 实现，不在代码 Default 里）。
    pub evaluate_persistent_login: bool,
    /// **P3-G1 注入链：出口防火墙配置**（裁决①）。引擎构造期灌入 [`crate::firewall::FirewallConfig`]，
    /// 经 `build_backend` → `from_launched` → [`crate::backend::cdp::spawn_fetch_firewall_loop`] 透传
    /// （**不再硬编码 `FirewallConfig::default()`**）。`Default` = `FirewallConfig::default()`（IP 封禁开
    /// 与跨域 POST 门控检测开）——默认即现行为，零回归。**域名 allowlist（`allow_etld1`/`deny_etld1`）
    /// 的真值注入是 D1 的活**（上层从 secret 的 per-pet `allowed_origins` 灌真策略，⑤共用真值）；G1 只打通
    /// 链路使 firewall **可注入**，facade 暂传 `default()`。
    pub firewall: crate::firewall::FirewallConfig,
    /// **W4d 持久登录：storage_state vault 灌入态**。上层从共享 vault
    /// （[`crate::vault::load_storage_state`]）解密读出的跨会话登录态 JSON（[`crate::StorageState`] 的
    /// `to_json` 形态：cookie + localStorage，DESIGN §17）。`Some` → 引擎在 page 建好后
    /// **启动注入**（`restore_cookies` context-bound 即时生效灌会话身份 + `restore_local_storage`
    /// origin-bound）实现**持久登录**（会话 A 登录→save vault→会话 B/重启 load vault→灌登录态恢复）。
    /// `None`（默认）= 不灌登录态（现行为零回归）。占位类型仍 `serde_json::Value`（与 vault JSON 同形态，
    /// 引擎内 `StorageState::from_json` 解析，形态不对则 warn 跳过不阻断启动）。
    pub storage_state: Option<serde_json::Value>,
    /// **P3-D2：出口审批通道接缝**（裁决④共用审批通道）。被 [`crate::firewall`] 门控（GatePost）的
    /// 跨域 POST / 出口到未授权域请求在 CDP `Fetch.requestPaused` 里**悬挂等裁决**——经本 approver
    /// （由 facade/网关注入，接到 GW2 的同一 pending 审批通道）取人在回路结果后 continueRequest（批准）/
    /// failRequest（拒绝）。`None`（默认）→ **fail-closed**：被门控请求一律拒绝（闭合 P2 跨域 POST 泄漏
    /// 窗口——detect-but-**fail**，绝不 detect-but-continue）。**真值注入是 facade/网关的活**；引擎只提供
    /// 接缝 + 悬挂机制 + fail-closed 兜底。
    pub egress_approver: Option<Arc<dyn crate::firewall::EgressApprover>>,
    /// **Known-secret exact-blackout registry** (debug-capture security keystone).
    ///
    /// Session-scoped set of the agent's own resolved secret plaintext values. The facade
    /// populates it on each successful `secret:NAME` resolution; the engine's debug serializers
    /// apply exact `String::replace` before any heuristic redaction, guaranteeing deterministic
    /// blackout of known secrets regardless of format, position, or entropy.
    ///
    /// See [`KnownSecretValues`] for security invariants and retention bounds.
    pub known_secret_values: KnownSecretValues,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `egress_approver` 是 trait 对象（无 Debug），手写 Debug 只标其有无（不打实现细节）。
        // `known_secret_values` prints count only (never the values themselves).
        let secret_count = self.known_secret_values.lock().map(|s| s.len()).unwrap_or(0);
        f.debug_struct("EngineConfig")
            .field("data_dir", &self.data_dir)
            .field("bundled_dir", &self.bundled_dir)
            .field("headful", &self.headful)
            .field("chrome_source", &self.chrome_source)
            .field("workspace_dir", &self.workspace_dir)
            .field("evaluate_full_power", &self.evaluate_full_power)
            .field("evaluate_persistent_login", &self.evaluate_persistent_login)
            .field("firewall", &self.firewall)
            .field("storage_state", &self.storage_state)
            .field("egress_approver", &self.egress_approver.as_ref().map(|_| "<approver>"))
            .field("known_secret_values", &format!("<{secret_count} values>"))
            .finish()
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            data_dir: std::env::temp_dir().join("nomifun-browser-data"),
            bundled_dir: None,
            headful: false,
            // 默认来源 = Managed（内置/下载 CfT 优先）= 现行为，零回归。
            chrome_source: ChromeSource::Managed,
            workspace_dir: None,
            // E3 default-deny：evaluate 全権默认 OFF（没有任何 session 默认能 evaluate）。
            evaluate_full_power: false,
            // SD-6 default-deny：persistent_login 代码级默认 false（产品 ON 由 factory host_default=true 实现）。
            evaluate_persistent_login: false,
            // P3-G1：默认 = FirewallConfig::default()（IP 封禁开 + 跨域 POST 门控检测开）= 现行为，零回归。
            firewall: crate::firewall::FirewallConfig::default(),
            storage_state: None,
            // P3-D2：默认无出口审批通道 → 被门控请求 fail-closed（拒绝）。facade/网关注入真 approver。
            egress_approver: None,
            // Known-secret blackout: default empty (no secrets known until facade resolves some).
            known_secret_values: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }
}

/// 端到端创建一个浏览器引擎：resolve → launch → connect → attach → page session。
///
/// 失败语义（绝不 panic）：
/// - 无当前平台 CfT 构建 / chrome 解析不到 → [`BrowserError::Unsupported`]（resolve 阶段）。
/// - 启动 / 连接 / 取 page session 失败 → [`BrowserError::Other`] / `SessionLost` 等（带诊断）。
///
/// 数据目录布局：chrome 下到 `<data_dir>/nomifun-browser/<version>/...`；专属
/// user-data-dir = `<data_dir>/profile`（红线：**绝不**指向用户真实 profile）。
pub async fn create_engine(config: EngineConfig) -> Result<Arc<dyn BrowserEngine>, BrowserError> {
    // 1) resolve chrome 可执行（env > 打包 > 数据目录 > 下载兜底；顺序由 chrome_source 编排）。
    let chrome_path = crate::acquire::resolve_chrome_path_with_source(
        &config.data_dir,
        config.bundled_dir.as_deref(),
        config.chrome_source,
    )
    .await?;

    // 2) 专属 user-data-dir（红线：非用户 profile；放在我们自己的 data_dir 下）。
    let user_data_dir = config.data_dir.join("profile");

    let launch_config = LaunchConfig {
        chrome_path,
        user_data_dir,
        headful: config.headful,
    };

    // E4 下载沙箱落点：per-pet workspace/downloads（有 workspace 上下文）或兜底 <data_dir>/downloads。
    // 二者都是**我们自己的隔离目录**——绝不落用户真实 Downloads（裁决⑩）。best-effort mkdir。
    let workspace = config
        .workspace_dir
        .clone()
        .unwrap_or_else(|| config.data_dir.clone());
    let download_dir = crate::download::ensure_download_dir(&workspace);

    // 3) launch（headless 由 display 决策）+ connect + attach + page session + setDownloadBehavior 沙箱。
    // P3-G1：把注入的 firewall 配置一路透传到 spawn_fetch_firewall_loop（不再硬编码 default）。
    // P3-D2：把注入的出口审批通道（egress_approver）透传到防火墙循环——被门控请求悬挂等其裁决
    //   （None → fail-closed）。
    // W4d：把 storage_state 透传——Some（上层从 vault load_storage_state 解出的登录态）→
    //   from_launched 在 page 建好后 restore_cookies + restore_local_storage 灌登录态（持久登录）；
    //   None（默认）→ 不灌（现行为零回归）。
    let backend = build_backend(
        &launch_config,
        Some(download_dir),
        config.workspace_dir.clone(),
        config.evaluate_full_power,
        config.evaluate_persistent_login,
        config.firewall,
        config.egress_approver,
        config.storage_state,
        config.known_secret_values,
    )
    .await?;
    Ok(Arc::new(backend))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_default_is_headless_with_temp_data_dir() {
        let c = EngineConfig::default();
        assert!(!c.headful);
        // 默认来源 = Managed（内置/下载 CfT 优先）= 现行为，零回归。
        assert_eq!(c.chrome_source, ChromeSource::Managed);
        assert!(c.bundled_dir.is_none());
        assert!(c.data_dir.ends_with("nomifun-browser-data"));
        // E4：无 per-pet 上下文时 workspace_dir 默认 None（兜底落 <data_dir>/downloads）。
        assert!(c.workspace_dir.is_none());
        // E3：evaluate 全权默认 OFF（default-deny）。
        assert!(!c.evaluate_full_power);
        // SD-6：evaluate_persistent_login 代码级默认 false（default-deny 基线；product ON 由
        // factory read_bool_pref host_default=true 实现，不在代码 Default 里）。
        assert!(
            !c.evaluate_persistent_login,
            "evaluate_persistent_login code-level Default must be false (default-deny base)"
        );
    }

    // ── P3-G1 注入链：EngineConfig firewall 字段 + W4 预留字段默认值（[纯逻辑]）──────────
    #[test]
    fn engine_config_default_firewall_is_firewall_default() {
        // 裁决①：EngineConfig.firewall 默认 = FirewallConfig::default()（IP 封禁开 + 跨域 POST 门控开）
        // = 现行为，零回归。
        let c = EngineConfig::default();
        assert_eq!(c.firewall, crate::firewall::FirewallConfig::default());
        assert!(c.firewall.block_private_ips);
        assert!(c.firewall.gate_cross_origin_post);
    }

    #[test]
    fn engine_config_default_storage_state_is_none() {
        // storage_state 默认 None（不灌登录态 = 现行为）。
        let c = EngineConfig::default();
        assert!(c.storage_state.is_none());
    }

    #[test]
    fn engine_config_accepts_custom_firewall() {
        // 注入一个**与 default 不同**的 firewall（关掉跨域 POST 门控）→ 字段如实保留。
        // 证明 EngineConfig 能承载注入值（链路下游透传由 build_backend 集成测试验）。
        let custom = crate::firewall::FirewallConfig {
            block_private_ips: true,
            gate_cross_origin_post: false,
            ..Default::default()
        };
        let c = EngineConfig {
            firewall: custom.clone(),
            ..Default::default()
        };
        assert_eq!(c.firewall, custom);
        assert_ne!(c.firewall, crate::firewall::FirewallConfig::default());
    }

    // ── 端到端集成冒烟（#[ignore]，本机/打包 chrome）───────────────────────────
    //
    // P0 首个端到端验证点：launch → navigate → screenshot。Windows 本机务必实跑一次：
    //   set NOMIFUN_CHROME_BINARY 指向系统 Chrome/Edge 的 chrome.exe（或走下载兜底），
    //   cargo nextest run -p nomi-browser-engine -- --ignored launch_navigate_screenshot
    // 跑完核对任务管理器无残留 chrome（Builder kill_on_drop / 清理网应自动清）。
    #[tokio::test]
    #[ignore = "需本机/打包 chrome，手动跑：set NOMIFUN_CHROME_BINARY 后 -- --ignored"]
    async fn launch_navigate_screenshot() {
        let engine = create_engine(EngineConfig::default())
            .await
            .expect("create_engine");

        let caps = engine.capabilities();
        assert_eq!(caps.engine, "chromium");
        assert!(caps.browser_ready);

        let nav = engine
            .navigate("https://example.com", false)
            .await
            .expect("navigate");
        assert!(
            nav.final_url.contains("example.com"),
            "final_url should contain example.com, got {}",
            nav.final_url
        );

        let png = engine.screenshot().await.expect("screenshot");
        assert!(png.len() > 1000, "screenshot too small: {} bytes", png.len());
        // PNG 文件头第 1..4 字节是 "PNG"（首字节 0x89）。
        assert_eq!(&png[1..4], b"PNG", "not a PNG: first bytes {:?}", &png[..8.min(png.len())]);

        // P3-K2：rendered_html 取**渲染后**原始 HTML（非 act 的 LLM 脱敏/包裹产物）。example.com
        // 是静态页，故 HTML 必含其正文（"Example Domain"）+ 原始标签（证明是未经 markdown 转换的
        // 真 outerHTML，且 BrowserFetcher 后续可拿它喂 html_to_markdown）。
        let html = engine.rendered_html().await.expect("rendered_html");
        assert!(
            html.contains("Example Domain"),
            "rendered_html should contain the page body, got {} bytes",
            html.len()
        );
        assert!(html.contains('<'), "rendered_html should be raw HTML, got: {html:.120}");
    }

    // 脏 profile reopen 循环（#[ignore]，本机 chrome）：证明
    // ① scrub 对真实磁盘上的 Preferences 生效（exit_type Crashed→Normal，只动这一键）；
    // ② create_engine→launch(内部 scrub)→navigate 在脏 profile 上健康(不因崩溃恢复卡死)。
    //   set NOMIFUN_CHROME_BINARY 后 cargo nextest run -p nomi-browser-engine -- --ignored
    //   dirty_profile_reopen_is_clean
    #[tokio::test]
    #[ignore = "需本机 chrome，手动跑：脏 profile reopen 循环"]
    async fn dirty_profile_reopen_is_clean() {
        let data_dir = std::env::temp_dir().join("nomi-browser-reopen-test");
        let _ = std::fs::remove_dir_all(&data_dir); // 干净起点
        let udd = data_dir.join("profile");
        let cfg = || EngineConfig { data_dir: data_dir.clone(), ..Default::default() };

        // 预置脏 profile：手写 Default/Preferences exit_type=Crashed（模拟上次崩溃/被硬杀的真实
        // 会话残留——chrome 真实使用后会写这个文件，被杀则停在 Crashed）。
        let prefs = crate::profile::preferences_path(&udd);
        std::fs::create_dir_all(prefs.parent().unwrap()).unwrap();
        std::fs::write(&prefs, r#"{"profile":{"exit_type":"Crashed","name":"Person 1"}}"#).unwrap();

        // ① launch 期用的 scrub I/O 函数对真实磁盘文件生效：Crashed→Normal，只动 exit_type。
        crate::profile::scrub_crash_markers(&udd).expect("scrub real prefs file");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&prefs).unwrap()).unwrap();
        assert_eq!(after["profile"]["exit_type"], "Normal", "scrub must clean exit_type");
        assert_eq!(after["profile"]["name"], "Person 1", "scrub must not touch sibling keys");

        // ② 再弄脏 → 完整 create_engine（launch 内部再 scrub）→ 在脏 profile 上导航健康。
        std::fs::write(&prefs, r#"{"profile":{"exit_type":"Crashed"}}"#).unwrap();
        let engine = create_engine(cfg())
            .await
            .expect("create_engine on dirty profile");
        engine
            .navigate("about:blank", false)
            .await
            .expect("navigate healthy after dirty-profile reopen");
    }
}
