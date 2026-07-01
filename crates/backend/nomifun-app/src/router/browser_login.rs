//! **Phase 2b: 「登录我的浏览器」** —— 让用户一键打开一个**可见**浏览器窗口,登录自己的站点一次,
//! 之后（默认静默的）agent 会话即复用该登录态。
//!
//! 登录态如何留存:所有会话共用**同一个磁盘 profile**(`<data_dir>/profile`,Chrome 原生把 cookie
//! 写盘),故在这个可见窗口里登录一次 → 磁盘 profile 记住 → 后续静默会话直接是登录态。关闭窗口前
//! 额外 best-effort 把登录态 capture 进加密 vault(Phase 2a),作为 profile 被清后的恢复备份。
//!
//! 生命周期:`open`(拉起可见窗口、返回)→ 用户在窗口里登录 →`close`(capture+save 备份、销毁引擎=
//! 关窗)。至多一个登录窗口(浏览器身份全局共享)。
//!
//! **红线不变**:用的仍是引擎**专属** user-data-dir(非用户真实 Chrome profile);来源(内置 CfT /
//! 系统 Chrome)由请求里的 `source` 决定,与 agent 一致。
//!
//! 约束:同一 profile 不能同时开两个 Chrome 实例——若此刻有 agent 会话正在跑浏览器,拉起会失败,
//! `open` 返回可读的错误信息(请先结束浏览器任务再登录)。仅 `browser-use` 构建有此路由;无显示器的
//! 服务器上引擎会被迫 headless(窗口不可见),故此功能面向桌面端。

#![cfg(feature = "browser-use")]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use nomi_browser_engine::{
    BrowserEngine, ChromeSource, EngineConfig, create_engine, save_storage_state,
    shared_storage_state_path,
};
use nomifun_api_types::ApiResponse;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// 登录窗口打开时落地的起始页。用户在 Chrome 地址栏输入自己的站点——我们**不**替用户导航到任何
/// 具体站点(about:blank + 原生地址栏)。
const LOGIN_START_URL: &str = "about:blank";

/// 持有唯一在活的登录浏览器引擎(open→close 之间)。全局共享(浏览器身份全局共享),同一时刻至多一个
/// 登录窗口。`Clone` 供 axum `State`(内部 `Arc` 共享)。
#[derive(Clone)]
pub(crate) struct BrowserLoginState {
    inner: Arc<BrowserLoginInner>,
}

struct BrowserLoginInner {
    /// 在活的登录引擎(`Some` = 窗口开着)。`close` 取走并 drop(=关窗)。
    session: Mutex<Option<Arc<dyn BrowserEngine>>>,
    /// 共享浏览器数据目录(与 agent 工具同源 `<app_config>/browser-data`),其下 `profile` 是共享 profile。
    data_dir: PathBuf,
    /// 打包 Chrome 资源目录(与 agent 一致),`None` 走 env>下载兜底。
    bundled_dir: Option<PathBuf>,
    /// 持久登录 vault 的机器绑定 key(与 secret vault 同一把)。
    key: [u8; 32],
}

impl BrowserLoginState {
    pub(crate) fn new(data_dir: PathBuf, bundled_dir: Option<PathBuf>, key: [u8; 32]) -> Self {
        Self {
            inner: Arc::new(BrowserLoginInner {
                session: Mutex::new(None),
                data_dir,
                bundled_dir,
                key,
            }),
        }
    }
}

/// `open` 请求体:`source` 镜像 `agent.browserUse.source`,使登录浏览器与 agent 用同一二进制。
#[derive(Deserialize)]
pub(crate) struct OpenLoginBody {
    #[serde(default)]
    source: String,
}

/// 登录窗口状态(open/close/status 共用)。
#[derive(Serialize)]
pub(crate) struct LoginStatus {
    /// 当前是否有登录窗口开着。
    active: bool,
    /// 结果说明码(`opened` / `already_open` / `closed` / `not_open` / `launch_failed:...`),
    /// 供 UI 展示/文案映射。
    message: Option<String>,
    /// close 时是否把登录态备份进了加密 vault(磁盘 profile 无论如何都已原生留存)。
    saved: bool,
}

/// POST /api/browser/login/open —— 拉起一个**可见**登录浏览器(共享 profile + 指定来源),落到起始页。
pub(crate) async fn open_browser_login(
    State(state): State<BrowserLoginState>,
    Json(body): Json<OpenLoginBody>,
) -> Json<ApiResponse<LoginStatus>> {
    let inner = &state.inner;
    let mut guard = inner.session.lock().await;
    if guard.is_some() {
        return Json(ApiResponse::ok(LoginStatus {
            active: true,
            message: Some("already_open".into()),
            saved: false,
        }));
    }
    let config = EngineConfig {
        data_dir: inner.data_dir.clone(),
        bundled_dir: inner.bundled_dir.clone(),
        // 可见窗口(有显示器时)让用户登录。无显示器的服务器会被迫 headless(窗口不可见)。
        headful: true,
        chrome_source: ChromeSource::from_source_str(&body.source),
        ..Default::default()
    };
    match create_engine(config).await {
        Ok(engine) => {
            // best-effort 落到起始页;失败不致命(窗口已起,用户可用地址栏)。
            let _ = engine.navigate(LOGIN_START_URL, false).await;
            *guard = Some(engine);
            Json(ApiResponse::ok(LoginStatus {
                active: true,
                message: Some("opened".into()),
                saved: false,
            }))
        }
        // 多为同 profile 已被 agent 会话占用(Chrome Singleton),或本机无 Chrome。给可读信息。
        Err(e) => Json(ApiResponse::ok(LoginStatus {
            active: false,
            message: Some(format!("launch_failed:{e}")),
            saved: false,
        })),
    }
}

/// POST /api/browser/login/close —— 关闭登录窗口:best-effort 把登录态备份进加密 vault,再销毁引擎(=关窗)。
pub(crate) async fn close_browser_login(
    State(state): State<BrowserLoginState>,
) -> Json<ApiResponse<LoginStatus>> {
    let inner = &state.inner;
    // 先取走引擎(释放锁),capture/save/drop 都在锁外做。
    let engine = { inner.session.lock().await.take() };
    let Some(engine) = engine else {
        return Json(ApiResponse::ok(LoginStatus {
            active: false,
            message: Some("not_open".into()),
            saved: false,
        }));
    };
    // best-effort 备份:capture 当前登录态 → 加密落共享 vault(profile 被清后可 seed 恢复)。
    // 空快照(无 cookie 无 localStorage)不落,避免覆盖好备份。磁盘 profile 已原生留存登录态。
    let saved = match engine.capture_storage_state().await {
        Ok(s) if !(s.cookies.is_empty() && s.local_storage.is_empty()) => {
            save_storage_state(&s, &shared_storage_state_path(&inner.data_dir), &inner.key).is_ok()
        }
        _ => false,
    };
    // 销毁引擎 = 关窗(CdpBackend Drop kill 掉 Chrome 子进程;cookie 已 flush 到磁盘 profile)。
    drop(engine);
    Json(ApiResponse::ok(LoginStatus {
        active: false,
        message: Some("closed".into()),
        saved,
    }))
}

/// GET /api/browser/login/status —— 当前是否有登录窗口开着(供 UI 切换按钮态/轮询)。
pub(crate) async fn browser_login_status(
    State(state): State<BrowserLoginState>,
) -> Json<ApiResponse<LoginStatus>> {
    let active = state.inner.session.lock().await.is_some();
    Json(ApiResponse::ok(LoginStatus {
        active,
        message: None,
        saved: false,
    }))
}
