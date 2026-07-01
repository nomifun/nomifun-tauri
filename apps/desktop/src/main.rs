// Prevent an extra console window on Windows in release builds. Debug builds
// keep the console so backend `tracing` logs are visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `nomifun-desktop` — the Tauri shell (replaces the Electron shell).
//!
//! Core idea (the whole point of this rewrite): there is NO spawned backend
//! binary. The unified Rust backend (`nomifun-app`, ex-`nomicore`) is linked
//! into THIS process and started in-process on a localhost port. The webview
//! loads the bundled SPA (`ui/dist`) and talks to `http://127.0.0.1:<port>/api`
//! exactly as it does today — so the renderer's ~295 HTTP calls are unchanged.
//!
//! ┌── nomifun-desktop (this process) ──────────────────────────┐
//! │  Tauri shell (window/tray/dialog/deep-link/updater)         │
//! │  └─ tokio task: nomifun_app embedded axum on 127.0.0.1:<p>  │
//! │  WebView2/WKWebView/WebKitGTK ── HTTP ──▶ 127.0.0.1:<p>/api │
//! └────────────────────────────────────────────────────────────┘

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Parser;
use nomifun_app::{DesktopServer, WebUiStatus};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;

mod relocate;

/// Build the webview initialization script. Injects the loopback backend port
/// (`window.__backendPort`), the OS tag, and the per-boot local-trust secret
/// (`window.__nomiLocalTrust`). Also installs `fetch` AND `XMLHttpRequest`
/// interceptors that attach the trust header to EVERY request bound for the
/// backend — so any code path (httpBridge, configService, raw `fetch`/XHR,
/// multipart uploads with progress events, …) is trusted without per-call
/// instrumentation, while requests to external origins are untouched (the
/// secret never leaks off-box). Runs before any page script.
fn webui_init_script(port: u16, trust_secret: &str) -> String {
    // `{:?}` emits properly quoted/escaped JS string literals.
    format!(
        r#"window.__backendPort = {port}; window.__os = {os:?}; window.__nomiLocalTrust = {secret:?};
(function () {{
  var secret = {secret:?};
  var origin = "http://127.0.0.1:" + {port};
  if (!secret) return;
  function isBackend(url) {{
    url = url || "";
    return url.indexOf(origin) === 0 || url.charAt(0) === "/";
  }}
  if (window.fetch && !window.__nomiFetchPatched) {{
    window.__nomiFetchPatched = true;
    var origFetch = window.fetch.bind(window);
    window.fetch = function (input, init) {{
      try {{
        var url = typeof input === "string" ? input : (input && input.url) || "";
        if (isBackend(url)) {{
          init = init || {{}};
          var h = new Headers((init && init.headers) || undefined);
          if (!h.has("x-nomi-local-trust")) h.set("x-nomi-local-trust", secret);
          init.headers = h;
          if (typeof input !== "string") input = url;
        }}
      }} catch (e) {{}}
      return origFetch(input, init);
    }};
  }}
  var XHR = window.XMLHttpRequest;
  if (XHR && XHR.prototype && !window.__nomiXhrPatched) {{
    window.__nomiXhrPatched = true;
    var proto = XHR.prototype;
    var origOpen = proto.open;
    var origSetHeader = proto.setRequestHeader;
    var origSend = proto.send;
    proto.open = function (method, url) {{
      this.__nomiUrl = url;
      return origOpen.apply(this, arguments);
    }};
    proto.setRequestHeader = function (name) {{
      try {{ (this.__nomiHeaders || (this.__nomiHeaders = {{}}))[String(name).toLowerCase()] = true; }} catch (e) {{}}
      return origSetHeader.apply(this, arguments);
    }};
    proto.send = function () {{
      try {{
        if (isBackend(this.__nomiUrl) && !(this.__nomiHeaders && this.__nomiHeaders["x-nomi-local-trust"])) {{
          this.setRequestHeader("x-nomi-local-trust", secret);
        }}
      }} catch (e) {{}}
      return origSend.apply(this, arguments);
    }};
  }}
}})();"#,
        os = std::env::consts::OS,
        secret = trust_secret,
        port = port,
    )
}

/// Resolve the bundled SPA directory (`ui/dist`) served to remote browsers by
/// the LAN listener. Probes a `NOMIFUN_WEBUI_DIST` override, several
/// resource-dir layouts (production bundle), then dev-tree relatives. A
/// candidate must contain `index.html` to be accepted; `None` means remote
/// browsers get the API only (logged as a warning).
fn resolve_webui_spa_dir(app: &tauri::App) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NOMIFUN_WEBUI_DIST") {
        let p = PathBuf::from(p);
        if p.join("index.html").is_file() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(res) = app.path().resource_dir() {
        candidates.push(res.join("webui-dist"));
        candidates.push(res.join("dist"));
        candidates.push(res.join("ui").join("dist"));
        // Tauri encodes `..` segments of a resource path as `_up_`.
        candidates.push(res.join("_up_").join("_up_").join("ui").join("dist"));
    }
    candidates.push(PathBuf::from("ui/dist"));
    candidates.push(PathBuf::from("../../ui/dist"));
    candidates.push(PathBuf::from("../ui/dist"));
    candidates.into_iter().find(|c| c.join("index.html").is_file())
}

/// Data root resolution, in priority order:
///
/// 1. `NOMIFUN_DATA_DIR` env — explicit override; the shell appends `/Nomi`
///    (semantics unchanged since the Electron era).
/// 2. The shared per-host default from `nomifun_app::cli::default_data_dir()`:
///    `%LOCALAPPDATA%\NomiFun\Nomi` on Windows, `~/Library/Application
///    Support/NomiFun/Nomi` on macOS, `$XDG_DATA_HOME/NomiFun/Nomi` on Linux,
///    with the historic `<system temp>/nomifun-data/Nomi` as the extreme
///    fallback (installs that used to land there are auto-relocated, see
///    `relocate.rs`). The web host and the `nomicore` bin resolve to the SAME
///    directory, so dev loops and the installed app share one state.
fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NOMIFUN_DATA_DIR") {
        return PathBuf::from(dir).join("Nomi");
    }
    nomifun_app::cli::default_data_dir()
}

/// Updater scaffold: ask the configured update endpoint whether a newer signed
/// release is available. Invoked from the renderer via
/// `invoke("check_for_updates")`. Returns the new version string, or `null` if
/// up to date. Inert until `plugins.updater.endpoints` in tauri.conf.json serves
/// a valid `latest.json` signed with the project key
/// (see apps/desktop/updater/README.md).
#[tauri::command]
async fn check_for_updates(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(update.version)),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Desired desktop-companion window, one per companion (multi-companion, spec §4.6).
#[derive(serde::Deserialize)]
struct CompanionWindowSpec {
    companion_id: String,
    enabled: bool,
}

// ---- WebUI / LAN remote-access lifecycle commands -------------------------
// The embedded backend always serves the app's own webview on loopback. These
// commands toggle the SEPARATE on-demand LAN listener (`0.0.0.0`) so remote
// browsers on the same network can reach the app by IP (with login). They are
// async so they run on Tauri's async runtime — never blocking the main thread.

/// Current WebUI/LAN serving status (running, port, LAN IP, URL).
///
/// Async because it resolves the persisted admin identity fresh from the DB
/// (via `status_snapshot`), so the panel shows the real username / password-set
/// state even when the LAN listener is stopped (e.g. right after a restart).
#[tauri::command]
async fn webui_get_status(server: tauri::State<'_, Arc<DesktopServer>>) -> Result<WebUiStatus, String> {
    let server = server.inner().clone();
    Ok(server.status_snapshot().await)
}

/// Start LAN serving (bind `0.0.0.0:25808`, fallback port if taken).
#[tauri::command]
async fn webui_start(server: tauri::State<'_, Arc<DesktopServer>>) -> Result<WebUiStatus, String> {
    let server = server.inner().clone();
    Ok(server.start_lan().await)
}

/// Stop LAN serving (the loopback listener / desktop webview are unaffected).
#[tauri::command]
async fn webui_stop(server: tauri::State<'_, Arc<DesktopServer>>) -> Result<WebUiStatus, String> {
    let server = server.inner().clone();
    Ok(server.stop_lan().await)
}

/// 持有当前生效的系统防休眠 assertion;`None`=允许休眠。
/// Drop `KeepAwake` 即释放 assertion,所以进程退出/关闭开关都能干净恢复正常电源行为。
/// Managed state holding the active OS sleep-inhibitor assertion (None = sleep allowed).
struct AwakeState(Mutex<Option<keepawake::KeepAwake>>);

/// 获取"保持唤醒"的 OS assertion:仅阻止系统空闲休眠(PreventUserIdleSystemSleep),
/// **不**阻止显示器空闲关闭 —— 等价 `caffeinate -i`(而非 `-di`)。电脑保持活动时屏幕仍可正常熄屏,
/// 既省电也避免长时间常亮对屏幕(尤其 OLED)的损耗;熄屏不影响定时任务运行。
/// `set_keep_awake` 与回归测试共用此单一来源。
/// Acquire the keep-awake assertion: inhibit system idle sleep only, while letting the display
/// sleep normally (≈ `caffeinate -i`, not `-di`) — saves power and avoids screen wear, and the
/// display turning off does NOT pause scheduled tasks. Single source shared with the test.
fn acquire_keep_awake() -> Result<keepawake::KeepAwake, String> {
    keepawake::Builder::default()
        .display(false) // 不持有 PreventUserIdleDisplaySleep:允许显示器空闲关闭(省电 + 护屏)
        .idle(true) // PreventUserIdleSystemSleep:系统保持唤醒;电池供电时同样生效
        .sleep(false) // PreventSystemSleep:已废弃 + 电池下被忽略,显式关闭
        .reason("NomiFun keep-awake enabled")
        .app_name("NomiFun")
        .app_reverse_domain("com.nomifun.desktop")
        .create()
        .map_err(|e| format!("failed to acquire keep-awake assertion: {e}"))
}

/// 开启/关闭"保持唤醒":开盖状态下阻止系统空闲休眠,但允许显示器照常熄屏(等价 `caffeinate -i`)。
/// macOS 硬限制:合盖属于"强制休眠"(forced sleep),任何 IOKit assertion 都拦不住(参见 Apple QA1340);
/// 合盖仍要运行,只能 clamshell 模式(插电 + 外接显示器 + 外接键鼠)或 root 级 `pmset disablesleep 1`。
/// 早先还持有 PreventUserIdleDisplaySleep(display=true)强制屏幕常亮,会阻止显示器关闭、徒增屏幕损耗,
/// 现已去掉;PreventSystemSleep(sleep)自 macOS 10.9 起已废弃且电池下被忽略,同样不用。
/// Keep-awake: with the lid OPEN, inhibit idle system sleep but let the display sleep (~`caffeinate -i`).
/// Lid-close is forced sleep that no assertion can block; the old display-on assertion (which blocked
/// the monitor from turning off) and the deprecated PreventSystemSleep are both gone.
#[tauri::command]
fn set_keep_awake(enabled: bool, state: tauri::State<'_, AwakeState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if enabled {
        if guard.is_none() {
            *guard = Some(acquire_keep_awake()?);
        }
    } else {
        *guard = None; // Drop 释放 assertion / Drop releases the assertion.
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod keep_awake_tests {
    use super::acquire_keep_awake;
    use std::process::Command;
    use std::thread::sleep;
    use std::time::Duration;

    /// 回归测试:保持唤醒必须只阻止系统空闲休眠,绝不阻止显示器关闭。
    /// 用真实 IOKit assertion + `pmset -g assertions` 验证 —— 只看本测试进程(按 pid 过滤)
    /// 自己持有的 assertion,因此不受同时运行的 App 实例或 `caffeinate` 干扰。
    /// Regression: keep-awake must hold PreventUserIdleSystemSleep but NOT
    /// PreventUserIdleDisplaySleep (the latter is what stops the monitor from turning off).
    #[test]
    fn holds_system_idle_assertion_but_not_display() {
        let handle = acquire_keep_awake().expect("acquire keep-awake assertion");
        let owner = format!("pid {}(", std::process::id());

        // assertion 注册是同步的,但留一点重试余量以防极偶发的可见性延迟。
        let mut ours: Vec<String> = Vec::new();
        for _ in 0..10 {
            let out = Command::new("pmset")
                .args(["-g", "assertions"])
                .output()
                .expect("run `pmset -g assertions`");
            ours = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains(&owner))
                .map(str::to_owned)
                .collect();
            if ours.iter().any(|l| l.contains("PreventUserIdleSystemSleep")) {
                break;
            }
            sleep(Duration::from_millis(50));
        }

        assert!(
            ours.iter().any(|l| l.contains("PreventUserIdleSystemSleep")),
            "keep-awake should hold PreventUserIdleSystemSleep; our assertions: {ours:?}"
        );
        assert!(
            !ours.iter().any(|l| l.contains("PreventUserIdleDisplaySleep")),
            "keep-awake must NOT hold PreventUserIdleDisplaySleep (it blocks the display from \
             turning off); our assertions: {ours:?}"
        );

        drop(handle);
    }
}

/// 关闭=收到托盘的护栏标志。仅在「真正退出」(托盘「退出」/`app.exit`)前置真,届时主窗口的
/// `CloseRequested` 处理停止拦截、放行关闭;默认关闭手势(标题栏 ×、系统关闭、Alt+F4)保持假,
/// 因此一律隐藏到托盘而非退出进程。
/// Set true just before a real quit so the main window's CloseRequested handler stops
/// intercepting; default close gestures leave it false and therefore hide to tray.
struct QuitFlag(AtomicBool);

/// 托盘菜单两项的句柄,留存以便前端在 UI 语言就绪后本地化标签(见 `set_tray_labels`)。
/// 创建时用英文兜底,确保渲染层挂载前托盘已可用。
/// Handles to the two tray menu items so the renderer can localize their labels once the
/// UI locale is known; built with English fallbacks so the tray works before the UI mounts.
struct TrayMenuItems {
    show: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

/// 把主窗口从托盘(或其他窗口背后)唤回:隐藏则显示、最小化则还原,并聚焦。
/// Bring the main window back from the tray: show if hidden, restore if minimized, then focus.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn should_show_main_window_for_macos_reopen(_has_visible_windows: bool) -> bool {
    true
}

fn handle_run_event(app: &tauri::AppHandle, event: tauri::RunEvent) {
    match event {
        // Real app exit: tray-quit's `app.exit(0)`, the `Destroyed`→`exit(0)`
        // path, macOS Cmd-Q, and last-window-closed all surface here. Close-to-tray
        // uses `api.prevent_close()` in the `CloseRequested` handler so the window
        // is merely hidden and this event NEVER fires for it — which makes it safe
        // to wipe every terminal session here (kill PTYs + delete rows) with no
        // QuitFlag guard. Blocks briefly (≤3s) so the wipe finishes before exit.
        tauri::RunEvent::ExitRequested { .. } => {
            if let Some(server) = app.try_state::<Arc<DesktopServer>>() {
                server.shutdown_terminals_blocking();
            }
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { has_visible_windows, .. } => {
            if should_show_main_window_for_macos_reopen(has_visible_windows) {
                show_main_window(app);
            }
        }
        _ => {
            let _ = app;
        }
    }
}

/// 本地化原生托盘菜单。渲染层在挂载时及语言切换时调用,传入翻译后的 `tray.showWindow` /
/// `tray.quit` 文案——Rust 侧无法自行解析 i18n,故创建时用英文兜底,随后采纳这些标签。
/// Localize the native tray menu: the renderer hands over translated labels on mount and on
/// language change (Rust can't resolve i18n itself, so it ships English fallbacks first).
#[tauri::command]
fn set_tray_labels(
    show: String,
    quit: String,
    items: tauri::State<'_, TrayMenuItems>,
) -> Result<(), String> {
    items.show.set_text(show).map_err(|e| e.to_string())?;
    items.quit.set_text(quit).map_err(|e| e.to_string())?;
    Ok(())
}

/// Reconcile the native desktop-companion window set (labels `companion-{companion_id}`) against
/// the desired specs sent by the main window (useCompanionWindowsSync):
///   - `companion-*` windows whose companion is gone or disabled → close;
///   - enabled companions without a window → create, hidden — the companion page shows the
///     window itself once its config loads (window autonomy, unchanged);
///   - windows already matching a desired spec are left untouched.
/// Async on purpose: creating a webview from a *sync* command can deadlock on
/// Windows (wry limitation).
#[tauri::command]
async fn sync_companion_windows(
    app: tauri::AppHandle,
    server: tauri::State<'_, Arc<DesktopServer>>,
    specs: Vec<CompanionWindowSpec>,
) -> Result<(), String> {
    use std::collections::HashSet;

    let known: HashSet<String> = specs
        .iter()
        .map(|s| format!("companion-{}", s.companion_id))
        .collect();
    let desired: HashSet<String> = specs
        .iter()
        .filter(|s| s.enabled)
        .map(|s| format!("companion-{}", s.companion_id))
        .collect();

    // Reconcile existing companion windows. Disabling a companion HIDES (keeps)
    // its window rather than closing it: destroy-then-recreate raced with the
    // async close/lingering and could leave a re-enabled companion with NO
    // visible window ("隐藏后再点显示，桌面伙伴再也起不来"). Only a companion that no
    // longer exists at all (deleted) is closed/destroyed.
    for (label, window) in app.webview_windows() {
        if !label.starts_with("companion-") {
            continue;
        }
        if !known.contains(&label) {
            if let Err(e) = window.close() {
                tracing::warn!(error = %e, label = %label, "failed to close removed companion window");
            }
        } else if !desired.contains(&label) {
            if let Err(e) = window.hide() {
                tracing::warn!(error = %e, label = %label, "failed to hide disabled companion window");
            }
        }
    }

    // Ensure every enabled companion has a VISIBLE window: show the existing one
    // (it may be hidden from a previous disable / right-click hide), or create it
    // (hidden; the companion page shows itself once its config loads).
    let init_script = webui_init_script(server.loopback_port(), server.local_trust_secret());
    for spec in specs.iter().filter(|s| s.enabled) {
        let label = format!("companion-{}", spec.companion_id);
        if let Some(window) = app.get_webview_window(&label) {
            // Only show a window that is actually HIDDEN. Tauri's `show()` maps to
            // tao `set_visible(true)` → `makeKeyAndOrderFront`, which makes the
            // window the macOS *key* window — stealing keyboard focus from the
            // main window — and it does this even when the window is already
            // visible (no `isVisible()` short-circuit anywhere in the chain). This
            // sync also fires on the MAIN window's own `focus` event
            // (useCompanionWindowsSync), so an unconditional `show()` turned every
            // main-window refocus into a focus-steal loop ("点按钮/打字总被夺焦").
            // Re-showing an already-visible companion has no visible effect anyway,
            // so skipping it is purely the removal of the unwanted re-key. An
            // `is_visible()` error biases toward showing (visibility correctness
            // outweighs a rare extra steal — a companion that won't appear is the
            // worse bug).
            if !window.is_visible().unwrap_or(false) {
                if let Err(e) = window.show() {
                    tracing::warn!(error = %e, label = %label, "failed to show enabled companion window");
                }
            }
            continue;
        }
        let url = format!("index.html#/companion?companionId={}", spec.companion_id);
        let builder =
            tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
                // Placeholder title for the brief pre-load frame; the companion page
                // overwrites it with the companion's custom name once its profile loads
                // (see setTitle in pages/companion/index.tsx). Never the lowercase engine id.
                .title("NomiFun")
                // Matches DEFAULT_DESK (characters/index.ts): figure + minimal chrome,
                // no reserved bubble headroom (the page grows the window on demand).
                // Keeping these in sync avoids a visible startup resize for built-ins.
                .inner_size(240.0, 214.0)
                .resizable(false)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .visible(false)
                .initialization_script(&init_script);
        // Show the freshly-built window from here rather than relying SOLELY on
        // the companion page's self-show (applyWindowState): on a FIRST enable
        // (no window existed, so we land in this create branch) the page init
        // can stall/fail (transient boot 5xx in its Promise.all, the configReady
        // gate, a wry build hiccup) and — since no later event re-enters the
        // create branch — the window would stay hidden forever ("开启桌面显示但
        // 桌面伙伴不出现"). The page's own show() is idempotent and still handles
        // position correction + later enable/disable toggles. A failed build
        // self-heals on the next sync.
        match builder.build() {
            Ok(window) => {
                // 创建即默认整窗穿透（安全侧）：JS 点击穿透轮询(useCompanionClickThrough)
                // 还没起、或 stale/dev 下 outerPosition 抛错卡死时，透明窗也不挡底层点击。
                // 命中立绘时由轮询切回 false。
                if let Err(e) = window.set_ignore_cursor_events(true) {
                    tracing::warn!(error = %e, label = %label, "failed to set ignore-cursor on companion window");
                }
                if let Err(e) = window.show() {
                    tracing::warn!(error = %e, label = %label, "failed to show created companion window");
                }
            }
            Err(e) => tracing::warn!(error = %e, label = %label, "failed to create companion window"),
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    // If an ACP agent CLI spawned this shell as an MCP stdio bridge
    // (`current_exe() mcp-requirement-stdio` etc.), run that helper and exit
    // BEFORE any runtime init, single-instance handling, or window creation.
    // Every host binary must honor these or the injected declaration tools
    // (requirement_complete / team / guide) never appear in the agent's session.
    if let Some(code) = nomifun_app::commands::run_mcp_stdio_subcommand_if_present() {
        return code;
    }

    // Env mutation + runtime init BEFORE Tauri builds its runtime/threads,
    // mirroring the nomicore bin's ordering. The relocation (legacy temp dir →
    // per-user app-data, one-shot) must run first of all: everything below —
    // runtime cache, backend cli, embedded server — keys off the data dir it
    // returns. On relocation failure it falls back to the legacy dir.
    let data_dir = relocate::effective_data_dir(default_data_dir());
    nomifun_runtime::init(&data_dir);
    // SAFETY: no worker threads exist yet (Tauri's runtime is built by .run()).
    let merged_path = unsafe { nomifun_runtime::enhance_process_path() };

    // Backend config. The desktop does NOT use `--local`: `DesktopServer::start`
    // runs the backend under `TrustLocalToken` (trusts only its own webview via
    // a per-boot secret) so the LAN listener can require login. Only the data
    // dir + log level flow from here; the listeners bind their own ports.
    let mut cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop"]);
    cli.data_dir = data_dir;
    // Opt-in verbose backend logging without a custom build, e.g.
    //   NOMI_LOG_LEVEL=debug            (everything)
    //   NOMI_LOG_LEVEL=info             (default)
    // At `debug`, the `nomi_providers` target logs the outgoing request body and
    // each SSE chunk, and `nomi_mcp` logs MCP connect results — exactly what is
    // needed to diagnose a provider/gateway stall. Console output appears in the
    // terminal that launched `tauri dev`; it is also written to the log files
    // under {data-dir}/logs/.
    if let Ok(level) = std::env::var("NOMI_LOG_LEVEL") {
        let level = level.trim();
        if !level.is_empty() {
            cli.log_level = Some(level.to_owned());
        }
    }

    let app = tauri::Builder::default()
        // single-instance MUST be the first plugin. With its `deep-link` feature
        // enabled (see Cargo.toml), it forwards a second instance's argv into the
        // deep-link plugin BEFORE invoking this callback, so `on_open_url` (wired
        // in setup) fires on its own. We still use the callback to surface the
        // existing window: a second launch usually means the app is hidden in the
        // tray (close-to-tray), so bring it back instead of silently no-op'ing.
        // (Schemes are statically configured in tauri.conf.json, so there is no
        // need to re-parse argv here.)
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None::<Vec<&str>>,
        ))
        .plugin(tauri_plugin_deep_link::init())
        .setup(move |app| {
            // Resolve the bundled SPA dir for serving the app shell to remote
            // browsers over the LAN listener (loopback webview loads via the
            // Tauri asset protocol and does not need this).
            let spa_dir = resolve_webui_spa_dir(app);
            // In dev, the desktop webview loads the live vite dev server; serving
            // the (stale) bundled `ui/dist` to remote browsers would desync them
            // from the desktop. So in dev the LAN listener proxies the SPA to vite
            // instead. In production this is None and the bundled dist is served.
            let dev_frontend_url: Option<String> = if tauri::is_dev() {
                app.config()
                    .build
                    .dev_url
                    .as_ref()
                    .map(|u| u.to_string())
                    .or_else(|| Some("http://localhost:5173".to_string()))
            } else {
                None
            };
            if spa_dir.is_none() && dev_frontend_url.is_none() {
                tracing::warn!(
                    "WebUI SPA directory not found — remote browsers would receive the API but no app shell"
                );
            }

            // Hand the backend control handle (port + trust secret + LAN
            // lifecycle) back to this thread once the loopback listener is bound.
            let (boot_tx, boot_rx) = std::sync::mpsc::channel::<Arc<DesktopServer>>();
            let backend_err_handle = app.handle().clone();
            let status_emit_handle = app.handle().clone();
            std::thread::Builder::new()
                .name("nomifun-backend".into())
                .spawn(move || {
                    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> anyhow::Result<()> {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()
                            .map_err(|e| anyhow::anyhow!("failed to build backend runtime: {e}"))?;
                        rt.block_on(async move {
                            let (server, _keep_alive) =
                                DesktopServer::start(&cli, &merged_path, spa_dir, dev_frontend_url).await?;
                            // Unblock the main thread's window build.
                            let _ = boot_tx.send(server.clone());
                            // Forward LAN status changes to the renderer. Holding
                            // `server` + `_keep_alive` here keeps the backend (and
                            // this runtime) alive for the process lifetime.
                            let mut rx = server.subscribe_status();
                            while rx.changed().await.is_ok() {
                                let status = rx.borrow().clone();
                                let _ = status_emit_handle.emit("webui://status-changed", status);
                            }
                            drop(_keep_alive);
                            Ok::<(), anyhow::Error>(())
                        })
                    }));
                    let error = match run {
                        Ok(Ok(())) => return, // clean shutdown
                        Ok(Err(e)) => format!("{e:#}"),
                        Err(panic) => panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "backend thread panicked".to_owned()),
                    };
                    tracing::error!(error = %error, "embedded backend exited with error");
                    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                    backend_err_handle
                        .dialog()
                        .message(error)
                        .title("NomiFun backend failed to start")
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                    backend_err_handle.exit(1);
                })
                .expect("failed to spawn backend thread");

            // Wait for the backend to bind its loopback listener (or fail) before
            // building the window — the init script needs the port + trust
            // secret. A recv error means the backend failed; it has already shown
            // a dialog and will exit, so we just stop building the window.
            let Ok(server) = boot_rx.recv() else {
                return Ok(());
            };
            let loopback_port = server.loopback_port();

            // Build the main window programmatically so we can inject the backend
            // port + local-trust secret via an INITIALIZATION SCRIPT — it runs
            // before any page script, so the renderer's first `getBaseUrl()` (and
            // its trust-header attach) always see them. Race-free (unlike
            // eval-after-load).
            //
            // Frameless on Windows/Linux: the React titlebar draws its own
            // min/max/close (via @tauri-apps/api/window) on the same row as the
            // app's nav buttons. macOS keeps native traffic-light buttons via the
            // Overlay title-bar style, with content extending under the bar.
            // resizable defaults to true, so edge-resize + Snap are retained even
            // without decorations on Windows.
            let init_script = webui_init_script(loopback_port, server.local_trust_secret());
            app.manage(server);
            let win_builder =
                tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App("index.html".into()))
                    .title("NomiFun")
                    .inner_size(1280.0, 832.0)
                    .min_inner_size(880.0, 600.0)
                    .initialization_script(&init_script);
            // macOS: Overlay makes the titlebar transparent + extends content under
            // it, but it does NOT hide the native title text. With the title still
            // set to "NomiFun", AppKit draws that string next to the traffic lights,
            // overlapping the React sidebar toggle. `hidden_title(true)` maps to
            // `setTitleVisibility(Hidden)` so the OS keeps the title for menus /
            // Mission Control while leaving the titlebar visually empty.
            //
            // Vertically center the traffic lights on the React toolbar's button
            // line. The React titlebar (`.app-titlebar--mac`, height 45px in
            // ui/.../titlebar.css) centers its 36px buttons at y≈22.5px from the
            // window top, but AppKit's default places the 16px lights at center
            // y≈16px — ~6.5px too high. tao's `inset_traffic_lights` (view.rs)
            // makes `y` the height of the button *container* (16 + y) and
            // bottom-anchors the lights in it with a ~10px margin, so the lights'
            // center-from-top works out to (y - 2). To land the center at 22.5px:
            // y = 24.5 (empirically verified via the Accessibility API).
            //
            // Horizontally, `x` is the left edge of the close button's frame
            // (tao sets `rect.origin.x = x` per button). That frame is 14px
            // wide with the visible 12px circle centered in it (1px each
            // side), so the circle's left gap from the window edge is x + 1.
            // Balance that gap with the vertical whitespace around the lights:
            // (45 - 12) / 2 = 16.5px above/below the circle, hence
            // x = 16.5 - 1 = 15.5. (AppKit's native ~8px inset assumes a 28px
            // titlebar and looks glued to the corner in a 45px one; Apple's
            // own apps use ~16-20px in tall toolbars.) The lights then span up
            // to zoom's right edge at 15.5 + 2*20 + 14 = 69.5px, still clear
            // of the React menu, which starts at 84px (8px titlebar padding +
            // 76px margin-left in Titlebar/index.tsx).
            // (`traffic_light_position` requires Overlay + decorations:true, both set.)
            #[cfg(target_os = "macos")]
            let win_builder = win_builder
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .hidden_title(true)
                .traffic_light_position(tauri::LogicalPosition::new(15.5, 24.5));
            #[cfg(not(target_os = "macos"))]
            let win_builder = win_builder.decorations(false);
            win_builder.build()?;

            // System tray. Closing the main window HIDES it here instead of
            // quitting (see the CloseRequested handler in on_window_event); the
            // process truly exits only via the tray's "退出" item. Left-click the
            // icon to bring the window back; right-click for the Show/Quit menu.
            // Labels are English fallbacks, adopted from the renderer's locale via
            // `set_tray_labels` once it mounts (the renderer always loads before
            // the user can close, so the first menu open is already localized).
            let tray_show = MenuItem::with_id(app, "tray-show", "Show NomiFun", true, None::<&str>)?;
            let tray_quit = MenuItem::with_id(app, "tray-quit", "Quit", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&tray_show, &tray_quit])?;
            app.manage(TrayMenuItems {
                show: tray_show.clone(),
                quit: tray_quit.clone(),
            });
            let mut tray_builder = TrayIconBuilder::with_id("nomi-tray")
                .tooltip("NomiFun")
                .menu(&tray_menu)
                // Left-click is reserved for "surface the window"; the menu is
                // right-click only (otherwise a left-click would both pop the menu
                // AND try to show the window).
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "tray-show" => show_main_window(app),
                    "tray-quit" => {
                        // Arm the quit guard FIRST, then exit — the CloseRequested
                        // handler checks this flag and stops hiding-to-tray.
                        app.state::<QuitFlag>().0.store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                });
            // Reuse the app's bundled window icon for the tray (no extra asset).
            if let Some(icon) = app.default_window_icon() {
                tray_builder = tray_builder.icon(icon.clone());
            }
            tray_builder.build(app)?;

            // Desktop-companion windows are NOT created here anymore. They are
            // multi-companion and dynamic: the main window's useCompanionWindowsSync hook
            // invokes `sync_companion_windows` (above) on boot and on companion
            // created/deleted/config-updated events, reconciling one
            // transparent always-on-top `companion-{companion_id}` window per enabled companion.

            // Wire deep-link open-url events to a Tauri event the renderer can
            // `listen()` to. `register_all()` is best-effort (some platforms /
            // dev contexts need it; ignore the error if it fails).
            let handle = app.handle().clone();
            let _ = app.deep_link().register_all();
            app.deep_link().on_open_url(move |event| {
                let urls: Vec<String> = event.urls().iter().map(|u| u.to_string()).collect();
                let _ = handle.emit("deep-link://received", urls);
            });
            Ok(())
        })
        // The ~38 OS-shell commands (window controls, tray, zoom, get-path,
        // feedback, auto-update status) register here as #[tauri::command]s (P3).
        .manage(AwakeState(Mutex::new(None)))
        .manage(QuitFlag(AtomicBool::new(false)))
        .invoke_handler(tauri::generate_handler![
            check_for_updates,
            sync_companion_windows,
            webui_get_status,
            webui_start,
            webui_stop,
            set_keep_awake,
            set_tray_labels
        ])
        // Close-to-tray is now the DEFAULT (and only) close behavior. Closing the
        // main window (titlebar ×, OS close, Alt+F4) hides it to the tray instead
        // of quitting — the agent, scheduled tasks, and companions keep running in
        // the background. The process exits ONLY via the tray's "退出" item, which
        // arms QuitFlag and calls app.exit(0); with the flag set we let the close
        // proceed and the Destroyed arm tears the process down (the always-on-top
        // companion windows would otherwise keep it — and a floating companion —
        // alive after the main window is gone).
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let quitting = window.app_handle().state::<QuitFlag>().0.load(Ordering::SeqCst);
                    if !quitting {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    window.app_handle().exit(0);
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // `Builder::run(context)` installs an empty app-level event callback. Build
    // manually so a Dock click after close-to-tray can surface the hidden main window.
    app.run(handle_run_event);

    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_reopen_surfaces_main_window_when_no_windows_are_visible() {
        assert!(should_show_main_window_for_macos_reopen(false));
    }

    #[test]
    fn macos_reopen_surfaces_main_window_even_when_companion_window_is_visible() {
        assert!(should_show_main_window_for_macos_reopen(true));
    }
}
