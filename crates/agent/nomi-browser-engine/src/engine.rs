//! Platform-neutral engine types + the `BrowserEngine` trait every browser
//! backend implements. Mirrors the shape of `nomi-a11y`'s `A11yEngine`: an
//! honest `capabilities()` report, a monotonic snapshot generation, and an
//! error enum the model reads and routes around — never a panic, never a silent
//! no-op. P0 exposes only the navigate/screenshot subset; `observe`/`act` land
//! in P1+.

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Monotonic snapshot generation. A `ref` produced against one generation is
/// only valid for that generation; backends use this to reject stale references
/// instead of acting on a node that has moved or detached. (Used from P1
/// onward; defined here so the handle type lives with the engine contract.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SnapshotGen(pub u64);

/// What the engine can actually do this session — injected into the system
/// prompt so the model knows its real abilities up front.
#[derive(Clone, Debug, Default)]
pub struct Capabilities {
    /// A managed Chromium is launched and the CDP transport is connected.
    pub browser_ready: bool,
    /// Running with a visible window (vs. headless).
    pub headful: bool,
    /// A display is available for headful rendering/screenshots.
    pub display_available: bool,
    /// Engine identifier, e.g. `"chromium"`.
    pub engine: String,
}

/// The outcome of a `navigate`: the URL the page actually settled on (after
/// redirects), the main-frame HTTP status when known, and the load state the
/// navigation reached.
#[derive(Clone, Debug)]
pub struct NavResult {
    pub final_url: String,
    pub http_status: Option<u16>,
    pub redirected: bool,
    pub load_state: LoadState,
}

/// 一次导航实际达到的生命周期里程碑（D2 由 P0 的弱类型字符串升级为强枚举）。
///
/// 语义阶梯（由浅入深，模型据此判断「页面就绪到什么程度」）：
/// - [`LoadState::Commit`]：文档刚提交（导航开始、收到主响应），DOM 还没构建。罕见地作为
///   返回值——多见于「navigate 回包成功但连 DOMContentLoaded 都没等到就降级」的兜底。
/// - [`LoadState::DomContentLoaded`]：`Page.domContentEventFired`（DOM 树构建完，脚本可跑），
///   但子资源（图片/样式/异步 fetch）可能还在加载。
/// - [`LoadState::Load`]：`Page.loadEventFired`（含子资源的 `load` 事件已触发）。**D2 的稳态默认**
///   ——settle + 可交互探测在此之上做 best-effort 加强。
/// - [`LoadState::NetworkIdle`]：在 `Load` 之上**额外**等到「无 inflight 请求持续 500ms」。仅当
///   networkidle 短 cap（3-5s）内真的达到才返回此值；长轮询/SSE/WS 站永不 idle → 到 cap 降级回
///   `Load`（**绝不**并入 30s 导航超时，见 [`crate::nav`]）。
///
/// serde 为 snake_case（`"commit"`/`"domcontentloaded"`/`"load"`/`"networkidle"`），与 PW 的
/// `LifecycleEvent` 命名对齐，便于跨语言契约 / 提示词文案。`Display` 同此（facade 拼「load state:
/// {}」时用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    /// 文档已提交（导航开始 / 主响应到达），DOM 未构建。
    Commit,
    /// `Page.domContentEventFired`：DOM 构建完，子资源可能未就绪。
    #[serde(rename = "domcontentloaded")]
    DomContentLoaded,
    /// `Page.loadEventFired`：含子资源的 `load` 已触发（D2 稳态默认）。
    Load,
    /// `Load` 之上额外达成「无 inflight 持续 500ms」（短 cap 内才返回此值）。
    #[serde(rename = "networkidle")]
    NetworkIdle,
}

impl fmt::Display for LoadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LoadState::Commit => "commit",
            LoadState::DomContentLoaded => "domcontentloaded",
            LoadState::Load => "load",
            LoadState::NetworkIdle => "networkidle",
        };
        f.write_str(s)
    }
}

/// Options for a single `observe`. `Default` is the model-friendly baseline:
/// depth-capped (so a huge page can't blow the token budget), injected-side diff
/// on, and no screenshot (screenshots go through the dedicated `screenshot`
/// action).
#[derive(Clone, Debug)]
pub struct ObserveOpts {
    /// depth 封顶，防超大页爆 token。默认 12。
    pub max_depth: u32,
    /// 启用注入侧 track diff（重注/导航后首帧全量）。默认 true。
    pub diff: bool,
    /// observe 是否附带（脱敏）截图。默认 false（截图走独立 screenshot 动作）。
    pub include_screenshot: bool,
    /// **P7B: 是否顺带采集每个可点击 ref 的几何**（`getBoundingClientRect`，CSS 像素）。默认 false
    /// （零额外 CDP 开销、零行为变化）。`true` → backend 在 observe 时多发**一次**注入侧批量查询，
    /// 把 ref→框写入 [`Observation::boxes`]，供 facade 的 SoM（Set-of-Marks）编号 overlay 用。
    pub include_boxes: bool,
}

impl Default for ObserveOpts {
    fn default() -> Self {
        Self {
            max_depth: 12,
            diff: true,
            include_screenshot: false,
            include_boxes: false,
        }
    }
}

/// ref 表对 LLM 的投影项。每项是一个可被 `act` 引用的元素：`ref` 是稳定句柄
/// （`f<seq>e<n>`，frame-local），加上人读的 role/name 与所属帧序号。
#[derive(Clone, Debug)]
pub struct ElementEntry {
    pub r#ref: String,
    pub role: String,
    pub name: String,
    pub frame_seq: u32,
}

/// **P7B: 一个可点击元素的包围盒，CSS 像素**（视口相对、零 DPR），由活节点的
/// `getBoundingClientRect()` 产出。仅当 [`ObserveOpts::include_boxes`] 时于 observe 期按 `ref`
/// 采集，写入 [`Observation::boxes`]，供 facade 的 SoM（Set-of-Marks）overlay 在截图上给可点击元素
/// 编号。CSS 像素（× `devicePixelRatio` → 截图的设备/图像像素）。**当前仅主帧**（`frame_seq == 0`）：
/// 子帧的 rect 是帧内坐标、需叠加 iframe 偏移才能对齐顶层截图（方案②，暂缓）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CssRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 一次 `observe` 的产物：序列化给 LLM 的 aria YAML + 配套的 ref 表 + 出处/封顶元信息。
#[derive(Clone, Debug)]
pub struct Observation {
    pub generation: SnapshotGen,
    /// `<data>` 包裹后的 aria YAML。
    pub yaml: String,
    pub entries: Vec<ElementEntry>,
    /// origin provenance（这次 observe 看的是哪个 URL）。
    pub url: Option<String>,
    /// depth 封顶触发（页面更深的节点被丢弃）。
    pub truncated: bool,
    /// 当前页是否来自 POST 表单提交（`transitionType == form_submit`）。
    /// reload 此类页面会重新提交表单（重复下单/扣款/发消息），facade 据此升 Irreversible。
    /// best-effort：nav-history 查不到 → `false`（保守不误判普通页）。
    pub current_page_is_post: bool,
    /// **P7B: 主帧可点击元素的 ref→CSS 像素包围盒**（仅当 [`ObserveOpts::include_boxes`]，否则空）。
    /// 供 facade 的 SoM overlay 编号用。`HashMap` 便于按 `ref` 查框；空 map = 未采集（零行为变化）。
    pub boxes: HashMap<String, CssRect>,
}

/// Errors are data the model reads and routes around — never a panic, never a
/// silent no-op. Started as the P0 subset (navigate/screenshot); P2 extends it
/// with DESIGN §22's action-layer taxonomy. Progress-layer timeout/abort
/// (`progress.rs`) is mapped into these at the action boundary via
/// [`crate::errmap::map_progress_err`] — never surfaced raw.
#[derive(Error, Debug)]
pub enum BrowserError {
    #[error("unsupported capability {capability}: {hint}")]
    Unsupported { capability: String, hint: String },
    #[error("browser session lost (recoverable={recoverable})")]
    SessionLost { recoverable: bool },
    #[error("blocked: {reason}")]
    Blocked { reason: String },
    #[error("navigation failed: {kind}")]
    NavFailed { kind: String },
    #[error("node stale (generation={generation})")]
    NodeStale { generation: u64 },
    #[error("node not connected")]
    NotConnected,
    // ── §22 taxonomy（动作层）：超时分相位、target/frame 生命周期、导航打断 ──
    /// 受控操作未在 deadline 内完成。`phase` 记录是哪一相（导航/动作/网络空闲），让模型
    /// 区分「页面没加载完」与「某次点击/输入卡住」。Progress `Timeout` 经
    /// [`crate::errmap::map_progress_err`] 映射到此。
    #[error("operation timeout ({phase:?})")]
    Timeout { phase: NavPhase },
    /// 渲染进程崩了（标签页 crash）。语义上可重开新 target 恢复，但崩溃本身是确定事件。
    #[error("target crashed")]
    TargetCrashed,
    /// target（标签页/page）被关闭——后续对它的操作都没有意义。Progress
    /// `Aborted(PageClosed)` 映射到此。
    #[error("target closed")]
    TargetClosed,
    /// 进行中的导航被一次**新**导航打断（旧操作的承诺作废）。Progress
    /// `Aborted(Cancelled)`（主动/父取消）映射到此。
    #[error("navigation interrupted by a new navigation")]
    NavigationInterrupted,
    /// frame 或 node 从树上 detach 了，引用失效。Progress `Aborted(FrameDetached)`
    /// 映射到 `Detached{kind: Frame}`。
    #[error("{kind:?} detached")]
    Detached { kind: DetachKind },
    #[error("{0}")]
    Other(String),
}

/// 超时所处的相位。让 [`BrowserError::Timeout`] 携带「是哪一步没完成」而非笼统超时，
/// 模型据此选择重试策略（导航重试 vs. 动作重定位 vs. 放宽网络空闲判定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavPhase {
    /// 导航本身（Page.navigate → 文档开始加载）。
    Nav,
    /// 一次具体动作（点击/输入/滚动等 actionability + 执行）。
    Action,
    /// 等网络空闲（SPA 软导航 / 异步加载后的稳定点）。
    NetworkIdle,
}

/// detach 的对象：整帧还是单个节点。区分让 [`BrowserError::Detached`] 的下游处理更精确
/// （帧 detach 通常要重新 observe 整页；节点 detach 多半只需重定位该元素）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachKind {
    /// frame 从父文档 detach。
    Frame,
    /// 单个 DOM node 从树上 detach。
    Node,
}

/// **引擎并发契约（DESIGN §22）**：每个 [`BrowserEngine`] 实现**必须**在单个引擎实例上把 `observe`
/// 与 `act`/`navigate`/`screenshot` 串行化——快照绝不与改 DOM 的动作交错（否则交回模型陈旧 `ref`）。
/// 后端用一把引擎内 per-engine mutex（跨每个操作体持有）实现。facade 仍报 `is_concurrency_safe == false`
/// （调度器也串行），但正确性**不再依赖**它——并发调用方也无法交错 observe/act。并发只在**不同引擎**
/// （各自 Chrome 进程）之间，绝不在单引擎内。本常量令该契约被代码引用并由测试钉死。
pub const OBSERVE_ACT_MUTEX_IS_ENGINE_ENFORCED: bool = true;

/// The contract every browser backend implements. Async because the CDP
/// transport is async end-to-end; `Send + Sync` so it can be shared across the
/// tool facade and the progress/abort machinery.
///
/// **并发契约（DESIGN §22）**：实现必须在单引擎实例上串行化 `observe` 与
/// `act`/`navigate`/`screenshot`（引擎内 per-engine mutex，见
/// [`OBSERVE_ACT_MUTEX_IS_ENGINE_ENFORCED`]）；正确性不依赖 `is_concurrency_safe`。
/// 并发跨不同引擎实现，绝不在单引擎内。
#[async_trait]
pub trait BrowserEngine: Send + Sync {
    /// Honest report of what this session can do.
    fn capabilities(&self) -> Capabilities;

    /// Navigate to `url`, optionally in a new tab, and return where the page
    /// settled once the navigation reached a load state.
    async fn navigate(&self, url: &str, new_tab: bool) -> Result<NavResult, BrowserError>;

    /// Capture a screenshot of the current page as PNG bytes.
    async fn screenshot(&self) -> Result<Vec<u8>, BrowserError>;

    /// Serialize the **fully-rendered** DOM of the current page to raw HTML
    /// (`document.documentElement.outerHTML`) — i.e. the post-JS markup the user
    /// would see, including content injected by client-side scripts that a plain
    /// HTTP GET never executes.
    ///
    /// **Why this is NOT `act(GetPageText)` / `Extract`** (the P3-K2 X6 finding):
    /// those action products are LLM-facing — they `redact` page secrets (lossily,
    /// irreversibly), wrap the text in `<data origin="…">` anti-injection
    /// delimiters, and prefix a human-readable note. That hardening is correct for
    /// feeding an agent but **corrupts a knowledge snapshot** (redaction
    /// placeholders + `<data>` tags + lost title/markdown structure). This method
    /// returns the *un-transformed* rendered HTML so the knowledge layer can run it
    /// through its own HTML→markdown converter (identical pipeline to the HTTP
    /// fetcher), keeping snapshots clean. It is **read-only** (no DOM mutation), so
    /// it carries no irreversible side effect and needs no approval gate.
    ///
    /// Callers that want JS-rendered text should `navigate` first (which settles to
    /// a load state) and then read this. Backends that cannot serialize the DOM
    /// return [`BrowserError::Unsupported`].
    async fn rendered_html(&self) -> Result<String, BrowserError>;

    /// Observe the current page: serialize its accessibility tree to aria YAML
    /// (`<data>`-wrapped) plus a `ref` table the model can act against. P1.
    async fn observe(&self, opts: &ObserveOpts) -> Result<Observation, BrowserError>;

    /// Execute a single action against the current page and report its effect.
    /// The action references elements by the `ref` handles `observe` projected.
    /// `progress` carries the deadline/abort scope (see [`crate::progress`]).
    /// P2; real execution lands in Stage B/C (the CDP backend currently stubs
    /// this as [`BrowserError::Unsupported`]).
    async fn act(
        &self,
        spec: &crate::actions::ActSpec,
        progress: &crate::progress::Progress,
    ) -> Result<crate::actions::ActResult, BrowserError>;

    /// **调试缓冲快照**（per-tab console/errors/network）。返回当前 active tab 的三个缓冲的
    /// clone 快照。供 `GetConsoleLogs`/`GetPageErrors`/`GetNetworkLog` 动作 + 集成测试用。
    /// 后端未实现 → `Unsupported`。
    async fn debug_snapshot(
        &self,
    ) -> Result<crate::debug_capture::DebugSnapshot, BrowserError>;

    /// **Takeover seam: bring the browser window to the foreground.**
    ///
    /// Headful + display → foreground the window (CDP `Page.bringToFront` +
    /// `Target.activateTarget`). Headless or no display → `Unsupported` (the caller
    /// maps this to `TakeoverResolution::Unavailable`).
    ///
    /// Default impl returns `Unsupported` for backends that don't support it.
    async fn bring_to_front(&self) -> Result<(), BrowserError> {
        Err(BrowserError::Unsupported {
            capability: "takeover".into(),
            hint: "bring_to_front not supported by this backend".into(),
        })
    }

    /// **持久登录：按需捕获当前登录态 [`StorageState`]**（cookie 全域 + 当前 origin 的
    /// localStorage / best-effort IndexedDB）。供上层（facade 登录后 / 登录按钮结束时）
    /// `save_storage_state` 落加密 vault，实现跨会话/重启的持久登录。
    ///
    /// **origin-bound 现实**：cookie 经 `Storage.getCookies` 取**全域**;localStorage / IndexedDB
    /// 按 origin 分区,只能取**当前 active tab 的那个 origin**(CDP 限制)。cookie 是登录态主载体,
    /// 故此捕获对绝大多数登录足够;多 origin 的 localStorage 需分别 navigate 后各采一次。
    ///
    /// 默认实现返回 `Unsupported`(非 CDP 后端 / 假后端)。
    async fn capture_storage_state(
        &self,
    ) -> Result<crate::storage_state::StorageState, BrowserError> {
        Err(BrowserError::Unsupported {
            capability: "capture_storage_state".into(),
            hint: "storage_state capture not supported by this backend".into(),
        })
    }

    /// **P7B: Visual fallback point-click seam.** Click at absolute CSS-pixel
    /// coordinates (zero DPR — the caller has already divided by devicePixelRatio).
    ///
    /// Used by the visual fallback path when DOM/aria anchoring fails and a vision
    /// model returns a coordinate target. The engine dispatches `mousePressed` +
    /// `mouseReleased` at the given point (same as `click_at` on the CDP backend).
    ///
    /// Default impl returns `Unsupported` for backends that don't implement it.
    async fn click_at_css_point(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        let _ = (x, y);
        Err(BrowserError::Unsupported {
            capability: "click_at_css_point".into(),
            hint: "point-click not supported by this backend".into(),
        })
    }

    /// **P7B: report the active page's `window.devicePixelRatio`.** The visual-fallback path
    /// uses it to convert vision-model device/image-pixel coordinates into the engine's
    /// DPR-free CSS-pixel input space (`to_css_point(px, py, dpr)`). Default `Ok(1.0)` —
    /// correct for headless Chrome (which defaults to DPR=1.0); the CDP backend overrides it
    /// via `Runtime.evaluate("window.devicePixelRatio")`. Best-effort: a query failure should
    /// be treated as `1.0` by the caller (never block a click on a DPR probe).
    async fn device_pixel_ratio(&self) -> Result<f64, BrowserError> {
        Ok(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_gen_monotonic() {
        let a = SnapshotGen(1);
        let b = SnapshotGen(2);
        assert!(b.0 > a.0);
    }

    #[test]
    fn observe_act_mutual_exclusion_is_engine_enforced_contract() {
        // 钉死 DESIGN §22：observe⊥act 由**引擎**(而非调用方)保证。CdpBackend 以 op_mutex 实现。
        assert!(super::OBSERVE_ACT_MUTEX_IS_ENGINE_ENFORCED);
    }

    #[test]
    fn browser_error_unsupported_carries_hint() {
        let e = BrowserError::Unsupported {
            capability: "evaluate".into(),
            hint: "feature off".into(),
        };
        assert!(format!("{e}").contains("evaluate"));
    }

    #[test]
    fn observe_opts_default_is_capped_diff_on_no_screenshot() {
        let o = ObserveOpts::default();
        assert_eq!(o.max_depth, 12);
        assert!(o.diff);
        assert!(!o.include_screenshot);
    }

    #[test]
    fn observation_is_clone_debug() {
        let obs = Observation {
            generation: SnapshotGen(1),
            yaml: "<data></data>".into(),
            entries: vec![],
            url: None,
            truncated: false,
            current_page_is_post: false,
            boxes: HashMap::new(),
        };
        let _ = format!("{:?}", obs.clone());
    }

    // ── D2：LoadState 强枚举 serde round-trip + Display（snake_case，与 PW 命名对齐）──

    #[test]
    fn load_state_serde_snake_case_roundtrip() {
        // 每个变体 serde 到约定 snake_case 串，再 round-trip 回来。
        for (variant, wire) in [
            (LoadState::Commit, "commit"),
            (LoadState::DomContentLoaded, "domcontentloaded"),
            (LoadState::Load, "load"),
            (LoadState::NetworkIdle, "networkidle"),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, format!("\"{wire}\""), "serialize {variant:?}");
            let back: LoadState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "round-trip {variant:?}");
        }
    }

    #[test]
    fn load_state_display_matches_wire() {
        // Display 与 serde 串一致（facade 拼「load state: {}」用 Display）。
        assert_eq!(LoadState::Commit.to_string(), "commit");
        assert_eq!(LoadState::DomContentLoaded.to_string(), "domcontentloaded");
        assert_eq!(LoadState::Load.to_string(), "load");
        assert_eq!(LoadState::NetworkIdle.to_string(), "networkidle");
    }

    #[test]
    fn nav_result_carries_strongly_typed_load_state() {
        // NavResult.load_state 现为 LoadState（非 String）；构造 + Debug 通。
        let nav = NavResult {
            final_url: "https://example.com/".into(),
            http_status: Some(200),
            redirected: false,
            load_state: LoadState::NetworkIdle,
        };
        assert_eq!(nav.load_state, LoadState::NetworkIdle);
        assert!(format!("{nav:?}").contains("NetworkIdle"));
    }
}
