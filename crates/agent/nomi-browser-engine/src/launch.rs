//! 托管启动 Chromium：经 [`nomifun_runtime::Builder`] spawn 解析到的 chrome，传随机
//! 调试端口（`--remote-debugging-port=0`，OS 分配）+ **专属 user-data-dir**（红线：永不
//! 碰用户 profile）+ [`crate::switches::chromium_switches`] 全量硬化开关，然后**轮询
//! `<user-data-dir>/DevToolsActivePort`** 拿到实际端口与 browser ws 路径，拼出
//! `ws://127.0.0.1:<port><path>` 交给 [`crate::transport::Connection`] connect。
//!
//! 为何读 DevToolsActivePort 而非 HTTP `/json/version`：免一次 HTTP（无需 `trust_env(false)`
//! 绕代理）、无需解析 JSON、且是 chrome 端口就绪的**权威信号**（文件出现即端口在监听）。
//!
//! 进程托管：`Builder::spawn` 已挂 `kill_on_drop(true)` + 三平台清理网（Windows Job
//! Object / Linux PDEATHSIG / macOS kqueue），故 engine 只要持有 child handle，退出即无
//! 残留 chrome。
//!
//! headless 决策：[`crate::display::display_available`] 为 false（无显示器：无头 server /
//! CI / SSH 无 X）→ 强制 `--headless=new`。headful 时给 `--window-position`（非主屏角）+
//! `--window-size`，避免遮主屏。

use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use tokio::process::Child;

use crate::engine::BrowserError;

/// 轮询 DevToolsActivePort 文件的最长等待（chrome 冷启 + 端口监听就绪）。仅 Windows ws 路径用。
#[cfg(windows)]
const PORT_FILE_TIMEOUT: Duration = Duration::from_secs(30);
/// 轮询间隔。仅 Windows ws 路径用。
#[cfg(windows)]
const PORT_FILE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 托管启动配置。`resolve_chrome_path`（Task 6）得到的可执行 + 专属数据目录 + headful。
#[derive(Clone, Debug)]
pub struct LaunchConfig {
    /// chrome 可执行绝对路径（来自 [`crate::acquire::resolve_chrome_path`]）。
    pub chrome_path: PathBuf,
    /// **专属** user-data-dir（红线：绝不指向用户真实 profile）。launch 会确保其存在。
    pub user_data_dir: PathBuf,
    /// 是否带可见窗口。注意：`display_available()==false` 时本标志被忽略，强制 headless。
    pub headful: bool,
}

/// 一次成功启动的产物：托管的 child handle（保活=保证退出清理）+ CDP 连接运输。
pub struct Launched {
    /// 托管的 chrome 进程句柄。engine 持有它；drop/kill 即清理整棵进程树。
    pub child: Child,
    /// CDP 连接运输（Unix=管道 / Windows=ws url）。
    pub transport: LaunchTransport,
}

/// CDP 连接运输。**Unix 生产用 `--remote-debugging-pipe`**（fd3/fd4；浏览器在本进程死亡——含
/// SIGKILL——时,内核关闭继承的 fd → Chromium 自行退出,跨平台父死自清的最优解,见
/// docs/superpowers/specs/browser-use/2026-06-19-macos-pdeath-pipe-transport-design.md）。
/// **Windows 生产用 ws url**（port + DevToolsActivePort + Job Object 清理；pipe 在 Windows 走继承
/// HANDLE,复杂且 Job Object 已内核级清理,故不转）。
pub enum LaunchTransport {
    /// Unix `--remote-debugging-pipe`：`cmd_writer`=我们写命令的管道写端（chrome 在 fd3 读）,
    /// `resp_reader`=我们读响应的管道读端（chrome 在 fd4 写）。交给 [`crate::transport::Connection::connect_pipe`]。
    #[cfg(unix)]
    Pipe {
        cmd_writer: std::os::fd::OwnedFd,
        resp_reader: std::os::fd::OwnedFd,
    },
    /// `ws://127.0.0.1:<port>/devtools/browser/<uuid>`，交给 [`crate::transport::Connection::connect`]。
    Ws { ws_url: String },
}

/// 构造 chrome 启动参数（纯函数，便于单测）。
///
/// - CDP 运输开关：Unix=`--remote-debugging-pipe`（fd3/fd4,浏览器父死自退）；Windows=
///   `--remote-debugging-port=0`（OS 分配 + DevToolsActivePort）。
/// - `--user-data-dir=<dir>`：专属数据目录（红线：非用户 profile）。
/// - [`crate::switches::chromium_switches`] 全量静态硬化开关。
/// - `--no-first-run` / `--no-default-browser-check`：免首启向导/默认浏览器询问。
/// - `--headless=new`：仅当 `force_headless`（无显示器或显式 headless）。
/// - headful（`!force_headless`）：`--window-position` + `--window-size`（非主屏角）。
/// - `--no-startup-window`：不自动开启动窗口（消除冗余 about:blank；受控页由 backend
///   `Target.createTarget` 单独建）。靠 `--remote-debugging-port` 触发的 REMOTE_DEBUGGING
///   keep-alive 保进程存活、不无窗口自退。
///
/// `force_headless` 由调用方按 `display_available()` 与 `LaunchConfig::headful` 算好后传入，
/// 使本函数保持纯逻辑、无平台/环境探测，单测可在任意宿主断言。
pub fn build_chrome_args(user_data_dir: &Path, force_headless: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // CDP 运输开关：Unix 用 `--remote-debugging-pipe`（fd3/fd4；浏览器在父死/管道 EOF 时自退,
    // 免疫 SIGKILL,见设计文档）;Windows 用 `--remote-debugging-port=0`（OS 分配 + DevToolsActivePort）。
    #[cfg(unix)]
    args.push("--remote-debugging-pipe".into());
    #[cfg(windows)]
    args.push("--remote-debugging-port=0".into());

    args.push(format!("--user-data-dir={}", user_data_dir.display()));

    // 静态硬化基线（零后台出站 / 容器防崩 / 截图可复现；Linux 含 dev-shm）。
    args.extend(crate::switches::chromium_switches());

    args.push("--no-first-run".into());
    args.push("--no-default-browser-check".into());

    if force_headless {
        // 无显示器强制无头；`=new` 是现代 headless（非旧 --headless），CDP 截图可用。
        args.push("--headless=new".into());
    } else {
        // headful：摆到非主屏角、给定窗口尺寸，避免遮挡主屏中心。
        args.push("--window-position=80,80".into());
        args.push("--window-size=1280,800".into());
    }

    // Linux 容器内 sandbox 常因缺 user-namespace 而启动失败；回退 --no-sandbox。
    // TODO(verify-linux): 容器 sandbox 探测/回退需实机核对（当前为无条件回退，偏保守），
    // 见 docs/superpowers/specs/browser-use/PLATFORM-VERIFICATION.md。
    #[cfg(target_os = "linux")]
    args.push("--no-sandbox".into());

    // **不自动开启动窗口/标签**：消除冗余的命令行起始标签——受控页由 backend
    // `Target.createTarget("about:blank")` 单独建（[`crate::backend::cdp`]），命令行再开一个就是
    // 多余的孤儿空白标签。改用 `--no-startup-window` 让 chrome 启动时不开任何窗口/标签。
    //
    // 为何不会因「无窗口」自退、也不影响 launch 轮询：本函数恒传 `--remote-debugging-port`
    // （上面），命中 Chromium 的 keep-alive 受支持组合——`(kNoStartupWindow || kHeadless) &&
    // (kRemoteDebuggingPort || kRemoteDebuggingPipe)` → `ScopedKeepAlive(REMOTE_DEBUGGING)`
    // 拴住进程直到显式 `Browser.close`（见 chrome/browser/devtools/chrome_devtools_manager_
    // delegate.cc）；且 DevToolsActivePort 在 socket bind 成功即写、与有无 window 无关（见
    // content/browser/devtools/devtools_http_handler.cc）→ launch_chrome 的端口轮询不受影响。
    // 平台无关的 Chromium 通用开关（keep-alive 逻辑同源、仅排除 ChromeOS，本仓不支持）。
    // TODO(verify-macos/linux): mac/linux 真机各冒烟一次确认（本机仅 Windows 已验），见
    // docs/superpowers/specs/browser-use/PLATFORM-VERIFICATION.md。
    args.push("--no-startup-window".into());

    args
}

/// 解析 DevToolsActivePort 文件内容 → `(port, ws_path)`。
///
/// chrome 在 `--remote-debugging-port=0` 下把实际监听信息写进
/// `<user-data-dir>/DevToolsActivePort`：
///   - 第 1 行：端口号（如 `54213`）；
///   - 第 2 行：browser ws 路径（如 `/devtools/browser/4f1c-...`）。
///
/// 返回 `Err(Other)` 给出明确诊断（行数不足 / 端口非数字）；不 panic。
pub fn parse_devtools_active_port(content: &str) -> Result<(u16, String), BrowserError> {
    let mut lines = content.lines();
    let port_line = lines
        .next()
        .ok_or_else(|| BrowserError::Other("DevToolsActivePort empty (no port line)".into()))?;
    let ws_path = lines
        .next()
        .ok_or_else(|| BrowserError::Other("DevToolsActivePort missing ws-path line".into()))?;

    let port: u16 = port_line.trim().parse().map_err(|e| {
        BrowserError::Other(format!(
            "DevToolsActivePort port line not a u16 ({port_line:?}): {e}"
        ))
    })?;
    if port == 0 {
        return Err(BrowserError::Other(
            "DevToolsActivePort reported port 0 (not yet bound)".into(),
        ));
    }

    let ws_path = ws_path.trim().to_string();
    if !ws_path.starts_with('/') {
        return Err(BrowserError::Other(format!(
            "DevToolsActivePort ws-path not absolute ({ws_path:?})"
        )));
    }
    Ok((port, ws_path))
}

/// 由端口 + ws 路径拼出 browser ws url（loopback v4）。
pub fn build_ws_url(port: u16, ws_path: &str) -> String {
    format!("ws://127.0.0.1:{port}{ws_path}")
}

/// 托管启动 chrome 并返回 child + CDP 连接运输。
///
/// 流程：确保 user-data-dir 存在 → scrub 脏 profile → 清 stale Singleton → 起 chrome。
/// **Unix**：`--remote-debugging-pipe`,经 fd3/fd4 即时连（无端口轮询；浏览器在父死/管道 EOF 时
/// 自退）;**Windows**：`--remote-debugging-port=0` + 轮询 DevToolsActivePort 拿端口/ws 路径。
/// `force_headless` 由调用方按 display 算好。
pub async fn launch_chrome(
    config: &LaunchConfig,
    force_headless: bool,
) -> Result<Launched, BrowserError> {
    // user-data-dir 必须存在（专属目录；红线已在 config 构造处保证非用户 profile）。
    std::fs::create_dir_all(&config.user_data_dir).map_err(|e| {
        BrowserError::Other(format!(
            "create user-data-dir {}: {e}",
            config.user_data_dir.display()
        ))
    })?;

    // **脏 profile 根治（keystone）**：上次 chrome 必被硬杀（kill_on_drop / Job Object / app 同步
    // exit），profile.exit_type 停在 "Crashed" → 下次启动弹「未正确关闭 / 恢复页面?」气泡 + 跑会话
    // 恢复（异常启动路径更易崩）。spawn 前（chrome 此刻必未运行）best-effort 洗回 "Normal"，是覆盖
    // 所有退出路径（含 crash/断电）的唯一可靠层。见 crate::profile 模块文档。
    if let Err(e) = crate::profile::scrub_crash_markers(&config.user_data_dir) {
        tracing::warn!(
            target: "nomi_browser_engine::launch",
            error = %e, dir = %config.user_data_dir.display(),
            "profile crash-marker scrub failed (best-effort; launch continues)"
        );
    }
    // mac/linux：顺手清 stale Singleton* 三件套（Windows 因 FILE_FLAG_DELETE_ON_CLOSE 无需）。
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    crate::profile::clear_stale_singleton(&config.user_data_dir);

    let mut args = build_chrome_args(&config.user_data_dir, force_headless);

    // Escape hatch（测试 / 高级排障）：`NOMI_CHROME_EXTRA_ARGS`（**每行一个参数**,故参数值可含空格,
    // 如 `--host-resolver-rules=MAP *.test 127.0.0.1`）追加到 chrome 启动参数（OOPIF 验证强制站点隔离 /
    // Emulation 调试旗标等）。生产默认未设 → 零影响。
    if let Ok(extra) = std::env::var("NOMI_CHROME_EXTRA_ARGS") {
        args.extend(
            extra
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }

    #[cfg(unix)]
    {
        launch_chrome_pipe(config, &args).await
    }
    #[cfg(windows)]
    {
        match launch_chrome_ws(config, &args).await {
            Ok(v) => Ok(v),
            Err(first) if should_retry_with_startup_page(&first, &args) => {
                tracing::warn!(
                    target: "nomi_browser_engine::launch",
                    error = %first,
                    "chrome exited before DevTools port was ready; retrying with an explicit startup page"
                );
                let fallback_args = chrome_args_with_startup_page(&args);
                launch_chrome_ws(config, &fallback_args).await.map_err(|second| {
                    BrowserError::Other(format!(
                        "chrome launch retry with startup page failed: {second}; first no-startup-window launch failed: {first}"
                    ))
                })
            }
            Err(e) => Err(e),
        }
    }
}

/// **Unix**：`--remote-debugging-pipe` 启动。建两条匿名管道,经 [`nomifun_runtime::Builder::inherit_fds`]
/// 把 chrome 端装到 fd3（读命令）/fd4（写响应）；我们持另两端交 [`crate::transport::Connection::connect_pipe`]。
/// 无端口轮询——管道即时可用,且浏览器在父死/管道 EOF 时自退（免疫 SIGKILL）。
#[cfg(unix)]
async fn launch_chrome_pipe(
    config: &LaunchConfig,
    args: &[String],
) -> Result<Launched, BrowserError> {
    // pipe_in：父写命令 → chrome 读（fd3）。pipe_out：chrome 写响应（fd4）→ 父读。
    let (chrome_cmd_read, our_cmd_write) = make_pipe()?;
    let (our_resp_read, chrome_resp_write) = make_pipe()?;

    let mut builder = nomifun_runtime::Builder::new(&config.chrome_path);
    builder
        .args(args)
        // chrome 的 stdout/stderr 我们不消费；null 掉避免污染父进程控制台。
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // chrome `--remote-debugging-pipe`：fd3 读命令、fd4 写响应。
        .inherit_fds(vec![(3, chrome_cmd_read), (4, chrome_resp_write)]);

    let mut child = builder.spawn().map_err(|e| {
        BrowserError::Other(format!(
            "spawn chrome {}: {e}",
            config.chrome_path.display()
        ))
    })?;

    // 快速失败：给 chrome 一小会儿；若立即退出（坏开关 / 缺依赖）立即报错,不必等首条 CDP 命令超时。
    tokio::time::sleep(Duration::from_millis(120)).await;
    if let Ok(Some(status)) = child.try_wait() {
        return Err(BrowserError::Other(format!(
            "chrome exited immediately after spawn (bad flags / missing deps?) status {status}"
        )));
    }

    Ok(Launched {
        child,
        transport: LaunchTransport::Pipe {
            cmd_writer: our_cmd_write,
            resp_reader: our_resp_read,
        },
    })
}

/// (unix) 建一条匿名管道 → `(读端, 写端)`,两端都设 `FD_CLOEXEC`。chrome 端经 Builder 的 dup2
/// shuffle 在 fd3/4 上清掉 CLOEXEC 以 survive exec；我们这端保持 CLOEXEC,绝不漏进 chrome 或其它 spawn。
#[cfg(unix)]
fn make_pipe() -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd), BrowserError> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) 成功时向数组写入恰好两个新建 owned fd。
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(BrowserError::Other(format!(
            "pipe(2): {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: pipe(2) 刚返回两个独占 fd,所有权移交 OwnedFd（drop 即 close）。
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_cloexec(&read)?;
    set_cloexec(&write)?;
    Ok((read, write))
}

#[cfg(unix)]
fn set_cloexec(fd: &std::os::fd::OwnedFd) -> Result<(), BrowserError> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    // SAFETY: F_GETFD/F_SETFD 在一个 owned fd 上,无前置条件。
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if flags < 0 {
        return Err(BrowserError::Other(format!(
            "fcntl F_GETFD: {}",
            std::io::Error::last_os_error()
        )));
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(BrowserError::Other(format!(
            "fcntl F_SETFD: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// **Windows**：`--remote-debugging-port=0` 启动,轮询 DevToolsActivePort 拿端口 + ws 路径,拼 ws url。
#[cfg(windows)]
async fn launch_chrome_ws(
    config: &LaunchConfig,
    args: &[String],
) -> Result<Launched, BrowserError> {
    // 删旧 DevToolsActivePort：复用目录时避免轮询读到上次启动的陈旧端口/路径。
    let port_file = config.user_data_dir.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&port_file);

    let mut builder = nomifun_runtime::Builder::new(&config.chrome_path);
    builder
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = builder.spawn().map_err(|e| {
        BrowserError::Other(format!(
            "spawn chrome {}: {e}",
            config.chrome_path.display()
        ))
    })?;

    // 轮询 DevToolsActivePort 直到出现且可解析，或 child 提前退出，或超时。
    let deadline = Instant::now() + PORT_FILE_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(BrowserError::Other(format!(
                "chrome exited before DevTools port was ready (status {status})"
            )));
        }
        if let Ok(content) = std::fs::read_to_string(&port_file) {
            if let Ok((port, ws_path)) = parse_devtools_active_port(&content) {
                let ws_url = build_ws_url(port, &ws_path);
                return Ok(Launched {
                    child,
                    transport: LaunchTransport::Ws { ws_url },
                });
            }
        }
        if Instant::now() >= deadline {
            let _ = nomifun_runtime::kill_process_tree(&mut child).await;
            return Err(BrowserError::Other(format!(
                "timed out after {}s waiting for DevToolsActivePort in {}",
                PORT_FILE_TIMEOUT.as_secs(),
                config.user_data_dir.display()
            )));
        }
        tokio::time::sleep(PORT_FILE_POLL_INTERVAL).await;
    }
}

#[cfg(windows)]
fn should_retry_with_startup_page(error: &BrowserError, args: &[String]) -> bool {
    args.iter().any(|a| a == "--no-startup-window")
        && matches!(
            error,
            BrowserError::Other(message)
                if message.contains("chrome exited before DevTools port was ready")
        )
}

#[cfg(windows)]
fn chrome_args_with_startup_page(args: &[String]) -> Vec<String> {
    let mut out: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--no-startup-window")
        .cloned()
        .collect();
    out.push("about:blank".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_include_port_user_data_dir_and_hardening() {
        let dir = Path::new("/tmp/nomi-udd");
        let args = build_chrome_args(dir, false);

        // 运输开关随平台：Unix=--remote-debugging-pipe（fd3/fd4 自死），Windows=--remote-debugging-port=0。
        #[cfg(unix)]
        assert!(
            args.iter().any(|a| a == "--remote-debugging-pipe"),
            "missing --remote-debugging-pipe flag: {args:?}"
        );
        #[cfg(windows)]
        assert!(
            args.iter().any(|a| a == "--remote-debugging-port=0"),
            "missing --remote-debugging-port=0 flag: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--user-data-dir=/tmp/nomi-udd"),
            "missing user-data-dir: {args:?}"
        );
        // 硬化基线关键项必须透传。
        assert!(args.iter().any(|a| a == "--disable-background-networking"));
        assert!(args.iter().any(|a| a == "--disable-component-update"));
        assert!(args.iter().any(|a| a.starts_with("--disable-features=")));
        assert!(args.iter().any(|a| a == "--no-first-run"));
        assert!(args.iter().any(|a| a == "--no-default-browser-check"));
        // 不自动开启动窗口（消除冗余命令行 about:blank；受控页由 backend createTarget 建）。
        assert!(args.iter().any(|a| a == "--no-startup-window"));
        assert!(
            !args.iter().any(|a| a == "about:blank"),
            "命令行不应再带 about:blank 起始页（受控页由 createTarget 建）: {args:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_retry_args_replace_no_startup_window_with_startup_page() {
        let args = vec![
            "--remote-debugging-port=0".to_string(),
            "--no-startup-window".to_string(),
            "--disable-background-networking".to_string(),
        ];
        let fallback = chrome_args_with_startup_page(&args);
        assert!(!fallback.iter().any(|a| a == "--no-startup-window"));
        assert!(fallback.iter().any(|a| a == "about:blank"));
        assert!(should_retry_with_startup_page(
            &BrowserError::Other(
                "chrome exited before DevTools port was ready (status exit code: 0)".into()
            ),
            &args
        ));
    }

    #[test]
    fn headless_flag_only_when_forced() {
        let dir = Path::new("/tmp/x");
        let headless = build_chrome_args(dir, true);
        assert!(
            headless.iter().any(|a| a == "--headless=new"),
            "force_headless must add --headless=new: {headless:?}"
        );
        // headless 时不该有 headful 的窗口摆位开关。
        assert!(!headless.iter().any(|a| a.starts_with("--window-position")));

        let headful = build_chrome_args(dir, false);
        assert!(
            !headful.iter().any(|a| a == "--headless=new"),
            "headful must NOT add --headless=new: {headful:?}"
        );
        // headful 时给窗口摆位/尺寸。
        assert!(headful.iter().any(|a| a.starts_with("--window-position")));
        assert!(headful.iter().any(|a| a.starts_with("--window-size")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_container_falls_back_to_no_sandbox() {
        // TODO(verify-linux): 当前无条件回退 --no-sandbox（偏保守）；容器探测见
        // docs/superpowers/specs/browser-use/PLATFORM-VERIFICATION.md。
        let args = build_chrome_args(Path::new("/tmp/x"), true);
        assert!(
            args.iter().any(|a| a == "--no-sandbox"),
            "linux must add --no-sandbox: {args:?}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_has_no_no_sandbox() {
        let args = build_chrome_args(Path::new("/tmp/x"), true);
        assert!(!args.iter().any(|a| a == "--no-sandbox"));
    }

    #[test]
    fn parse_active_port_two_lines() {
        let content = "54213\n/devtools/browser/4f1c0a2b-aaaa-bbbb-cccc-ddddeeeeffff\n";
        let (port, path) = parse_devtools_active_port(content).unwrap();
        assert_eq!(port, 54213);
        assert_eq!(
            path,
            "/devtools/browser/4f1c0a2b-aaaa-bbbb-cccc-ddddeeeeffff"
        );
        assert_eq!(
            build_ws_url(port, &path),
            "ws://127.0.0.1:54213/devtools/browser/4f1c0a2b-aaaa-bbbb-cccc-ddddeeeeffff"
        );
    }

    #[test]
    fn parse_active_port_trims_whitespace() {
        // chrome 可能不带末尾换行；也容忍行内多余空白。
        let content = "  9333  \n  /devtools/browser/x  ";
        let (port, path) = parse_devtools_active_port(content).unwrap();
        assert_eq!(port, 9333);
        assert_eq!(path, "/devtools/browser/x");
    }

    #[test]
    fn parse_active_port_rejects_missing_lines() {
        assert!(parse_devtools_active_port("").is_err());
        assert!(parse_devtools_active_port("54213").is_err()); // 缺第二行
    }

    #[test]
    fn parse_active_port_rejects_bad_port() {
        assert!(parse_devtools_active_port("notaport\n/devtools/browser/x").is_err());
        assert!(parse_devtools_active_port("0\n/devtools/browser/x").is_err()); // 0=未绑定
    }

    #[test]
    fn parse_active_port_rejects_non_absolute_ws_path() {
        assert!(parse_devtools_active_port("9333\ndevtools/browser/x").is_err());
    }
}
