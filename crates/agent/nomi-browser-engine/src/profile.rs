//! 脏 profile 崩溃标记 scrub：每次 spawn chrome **前**，把专属 user-data-dir 的
//! `<profile>/Default/Preferences` 里 chromium 记录的「上次崩溃/被杀」标记改回干净，
//! 根治非优雅退出（TerminateProcess / Job Object 硬杀 / app crash / 断电）后下次启动弹
//! 「Chrome 未正确关闭 / 恢复页面?」气泡 + 跑会话恢复。
//!
//! ## 为什么这是 keystone（唯一在所有退出路径都生效的层）
//! 本引擎的 chrome 进程**永远是被硬杀**的（`launch.rs` 的 `kill_on_drop` + Windows Job
//! Object `KILL_ON_JOB_CLOSE`），且 app 退出时后端线程被同步 `exit(0)` 硬杀、跑不到任何
//! 异步优雅关闭。优雅 `Browser.close`（写回干净退出）在当前架构下**无可达调用点**，故
//! 真正的根治是：承认「下次必是脏 profile」，在**下次 launch 前**把崩溃标记洗干净。
//!
//! ## 权威来源（Chromium 源码核实，见 docs/superpowers/specs/browser-use）
//! - 键 `profile.exit_type`（C++ 常量 `prefs::kSessionExitType`，`chrome/common/pref_names.h`）。
//!   取值 `"Normal"`（干净）/ `"Crashed"`（崩溃被杀）/ `"SessionEnded"`（系统强退），见
//!   `chrome/browser/sessions/exit_type_service.cc`。
//! - **时序**：chromium 启动时**立即把 `exit_type` 写成 `"Crashed"`**，只有走完整干净关闭
//!   才回写 `"Normal"`——故被硬杀必留 `"Crashed"`。下次启动 `HasPendingUncleanExit()`
//!   （`startup_browser_creator.cc`）见 `exit_type==Crashed` 即武装气泡。把它改回 `"Normal"`
//!   → 气泡闸门不触发 + 不跑会话恢复。
//! - **无可靠命令行开关**：`--disable-session-crashed-bubble` 已从源码树删除；
//!   `--hide-crash-restore-bubble` 仅 ChromeOS full-restore 生效（桌面 Windows no-op）。
//!   故唯一权威手段是改 `exit_type`（等价 ChromeDriver `PrepareUserDataDir` 种子化）。
//!
//! ## 红线
//! 只动 `profile.exit_type` 这**一个**键，绝不动其它 pref（cookie / localStorage 等登录态
//! 全保留）。文件不存在 = 首启，跳过；JSON 损坏 = best-effort 不致命（warn 后照常启动）。
//! 必须在 chrome **未运行**时改（launch 前，本引擎专属 dir 同一时刻只一个 chrome）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 默认 profile 子目录。本引擎单 profile 启动（不传 `--profile-directory`），故恒 `Default`。
pub const DEFAULT_PROFILE_SUBDIR: &str = "Default";
/// profile 偏好文件名。
pub const PREFERENCES_FILE: &str = "Preferences";
/// `profile.exit_type` 的干净值（对应 Chromium `ExitType::kClean`）。
const EXIT_TYPE_NORMAL: &str = "Normal";

/// `<user-data-dir>/Default/Preferences` 的绝对路径。
pub fn preferences_path(user_data_dir: &Path) -> PathBuf {
    user_data_dir.join(DEFAULT_PROFILE_SUBDIR).join(PREFERENCES_FILE)
}

/// **纯逻辑**：把一段 Preferences JSON 文本的崩溃标记改成「干净退出」。
///
/// - `Ok(Some(new_text))`：`profile.exit_type` 原本非 `"Normal"`，已改写，需回写。
/// - `Ok(None)`：已是 `"Normal"`（免无谓写盘）。
/// - `Err(msg)`：JSON 解析/结构异常（调用方 best-effort：warn 后照常启动）。
///
/// 只插入/改写 `profile.exit_type`，其它键（含 `profile` 下的兄弟键）原样保留。
pub fn scrub_prefs_json(text: &str) -> Result<Option<String>, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("parse Preferences: {e}"))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| "Preferences root not a JSON object".to_string())?;
    // `profile` 不存在则建空对象（边角：极简/损坏 prefs）；存在但非对象 = 结构异常 → Err。
    let profile = obj
        .entry("profile")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| "Preferences `profile` is not a JSON object".to_string())?;

    if profile.get("exit_type").and_then(|x| x.as_str()) == Some(EXIT_TYPE_NORMAL) {
        return Ok(None); // 已干净
    }
    profile.insert("exit_type".into(), serde_json::json!(EXIT_TYPE_NORMAL));
    serde_json::to_string(&v)
        .map(Some)
        .map_err(|e| format!("serialize Preferences: {e}"))
}

/// 薄 I/O 包装：读 `<user-data-dir>/Default/Preferences` → [`scrub_prefs_json`] → 原子回写。
///
/// **best-effort 语义**（绝不阻断启动）：
/// - 文件不存在（首启，chrome 尚未建过 profile）→ `Ok(())`，跳过。
/// - JSON 损坏 / 结构异常 → warn + `Ok(())`（照常启动；最坏情况只是弹一次气泡）。
/// - 仅当真有改动时写盘（temp + rename 原子替换，避免 chrome 读到半截——虽然此刻
///   chrome 必未运行，原子写仍是稳妥习惯）。
///
/// 返回 `Err` 仅限**非 NotFound 的读 I/O 错误**（如权限），交调用方 warn。
pub fn scrub_crash_markers(user_data_dir: &Path) -> std::io::Result<()> {
    let path = preferences_path(user_data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // 首启，无 profile
        Err(e) => return Err(e),
    };
    match scrub_prefs_json(&text) {
        Ok(Some(new_text)) => {
            // 原子回写：写同目录临时文件再 rename（同卷 rename 是原子替换）。
            let tmp = path.with_extension("nomi-scrub.tmp");
            std::fs::write(&tmp, new_text)?;
            std::fs::rename(&tmp, &path)?;
            Ok(())
        }
        Ok(None) => Ok(()), // 已干净
        Err(msg) => {
            tracing::warn!(
                target: "nomi_browser_engine::profile",
                error = %msg, path = %path.display(),
                "Preferences crash-marker scrub skipped (best-effort; launch continues)"
            );
            Ok(())
        }
    }
}

/// macOS/Linux：清理 stale `Singleton*` 三件套（symlink → `hostname-pid`，硬杀残留）。
///
/// Windows **不需要**：其单实例锁是 `lockfile`（`FILE_FLAG_DELETE_ON_CLOSE`），进程被杀时
/// 内核自动删除；单实例发现靠命名互斥量 + 隐藏消息窗口（随进程消失），无 stale 文件锁。
///
/// chrome 通常能自愈 stale lock（检查 pid/hostname 后破链重建），仅跨主机共享 profile 等
/// 边角才阻塞——我们用专属本机 dir，删它纯属兜底。
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn clear_stale_singleton(user_data_dir: &Path) {
    // TODO(verify-macos/linux)：本机仅 Windows；mac/linux 上的 stale lock 行为待实机核对，
    // 见 docs/superpowers/specs/browser-use/PLATFORM-VERIFICATION.md。
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(user_data_dir.join(name));
    }
}

/// **[纯逻辑] 判定一个 per-instance profile 目录是否 stale（可被启动 GC 回收）**：其 `mtime` 早于
/// `now - max_age`。用于清理上次运行崩溃/被硬杀、未走引擎 Drop 清理的孤儿 profile 目录。
///
/// - `mtime` 在未来（时钟回拨）→ 保守视为「新」（不删），避免误删。
/// - `age >= max_age` → stale。纯函数（不碰 FS），故 GC 判定可脱离时钟/文件系统单测。
pub fn is_stale_profile(dir_mtime: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    match now.duration_since(dir_mtime) {
        Ok(age) => age >= max_age,
        Err(_) => false, // mtime 在未来（时钟回拨）→ 保守视为新，不删
    }
}

/// **启动时 GC per-instance profile 目录**：删除 `profiles_root` 下 `mtime` 早于 `now - max_age` 的
/// **子目录**（上次运行崩溃/硬杀遗留、未经引擎 Drop 清理的孤儿）。
///
/// 语义：best-effort（读不到 root / 删失败 → 跳过，绝不 panic / 阻断启动）；**只删目录**，非目录项与
/// `profiles_root` 本身原样保留。正常退出的 per-instance profile 已由 [`crate::backend::cdp::CdpBackend`]
/// 的 ephemeral Drop 清理，本函数是孤儿兜底 + 磁盘增长上界。`max_age` 取保守值（如 1 小时）以免误删
/// 另一进程（如 MCP stdio 桥）刚建的活目录。
pub fn gc_stale_profiles(profiles_root: &Path, max_age: Duration) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(profiles_root) else {
        return; // root 不存在（首次运行）/ 不可读 → 无事可做
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue; // 只回收目录（非目录项不碰）
        }
        let mtime = meta.modified().unwrap_or(now);
        if is_stale_profile(mtime, now, max_age) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_stale_profile_flags_dirs_older_than_max_age() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000); // age 9000s
        let recent = SystemTime::UNIX_EPOCH + Duration::from_secs(9_990); // age 10s
        assert!(
            is_stale_profile(old, now, Duration::from_secs(3600)),
            "a dir older than max_age is stale"
        );
        assert!(
            !is_stale_profile(recent, now, Duration::from_secs(3600)),
            "a dir younger than max_age is not stale"
        );
        // 时钟回拨（mtime 在未来）→ 保守视为新，不删。
        let future = now + Duration::from_secs(100);
        assert!(
            !is_stale_profile(future, now, Duration::from_secs(3600)),
            "a future mtime (clock skew) is treated as fresh"
        );
    }

    #[test]
    fn gc_stale_profiles_removes_stale_dirs_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("profiles");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("keep.txt"), "x").unwrap();

        // max_age = 0 → 一切皆 stale → 两个子目录都删；非目录项 + root 本身保留。
        gc_stale_profiles(&root, Duration::from_secs(0));
        assert!(!root.join("a").exists(), "stale dir a removed");
        assert!(!root.join("b").exists(), "stale dir b removed");
        assert!(root.join("keep.txt").exists(), "non-dir entries left untouched");
        assert!(root.exists(), "profiles root itself is preserved");

        // max_age 很大 → 刚建的目录不删（不误删活目录）。
        std::fs::create_dir_all(root.join("c")).unwrap();
        gc_stale_profiles(&root, Duration::from_secs(3600));
        assert!(root.join("c").exists(), "a fresh dir survives a large max_age");

        // root 不存在 → 静默无操作（首次运行）。
        gc_stale_profiles(&tmp.path().join("nope"), Duration::from_secs(0));
    }

    #[test]
    fn preferences_path_is_default_subdir() {
        let p = preferences_path(Path::new("/data/profile"));
        assert!(p.ends_with("Default/Preferences") || p.ends_with("Default\\Preferences"));
    }

    #[test]
    fn scrub_rewrites_crashed_to_normal() {
        let dirty = r#"{"profile":{"exit_type":"Crashed","name":"Person 1"},"other":42}"#;
        let out = scrub_prefs_json(dirty).unwrap().expect("changed → Some");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profile"]["exit_type"], "Normal");
        // 红线：只动 exit_type，兄弟键 + 顶层键原样保留。
        assert_eq!(v["profile"]["name"], "Person 1");
        assert_eq!(v["other"], 42);
    }

    #[test]
    fn scrub_rewrites_session_ended_to_normal() {
        let dirty = r#"{"profile":{"exit_type":"SessionEnded"}}"#;
        let out = scrub_prefs_json(dirty).unwrap().expect("changed → Some");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profile"]["exit_type"], "Normal");
    }

    #[test]
    fn scrub_noop_when_already_normal() {
        let clean = r#"{"profile":{"exit_type":"Normal"}}"#;
        assert!(scrub_prefs_json(clean).unwrap().is_none(), "already clean → None (no write)");
    }

    #[test]
    fn scrub_inserts_exit_type_when_profile_lacks_it() {
        let no_key = r#"{"profile":{"name":"x"}}"#;
        let out = scrub_prefs_json(no_key).unwrap().expect("inserted → Some");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profile"]["exit_type"], "Normal");
        assert_eq!(v["profile"]["name"], "x");
    }

    #[test]
    fn scrub_creates_profile_object_when_missing() {
        let no_profile = r#"{"some":"thing"}"#;
        let out = scrub_prefs_json(no_profile).unwrap().expect("created profile → Some");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["profile"]["exit_type"], "Normal");
        assert_eq!(v["some"], "thing");
    }

    #[test]
    fn scrub_errs_on_bad_json() {
        assert!(scrub_prefs_json("not json").is_err());
    }

    #[test]
    fn scrub_errs_when_profile_is_not_object() {
        // profile 存在但是字符串 → 结构异常 → Err（调用方 best-effort 吞掉）。
        assert!(scrub_prefs_json(r#"{"profile":"oops"}"#).is_err());
    }

    #[test]
    fn scrub_crash_markers_roundtrips_on_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let udd = tmp.path();
        let prefs = preferences_path(udd);
        std::fs::create_dir_all(prefs.parent().unwrap()).unwrap();
        std::fs::write(&prefs, r#"{"profile":{"exit_type":"Crashed"}}"#).unwrap();

        scrub_crash_markers(udd).expect("scrub ok");

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&prefs).unwrap()).unwrap();
        assert_eq!(after["profile"]["exit_type"], "Normal");
        // 临时文件不残留。
        assert!(!prefs.with_extension("nomi-scrub.tmp").exists());
    }

    #[test]
    fn scrub_crash_markers_skips_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        // 无 Default/Preferences（首启）→ Ok，不报错、不建文件。
        scrub_crash_markers(tmp.path()).expect("missing file is benign");
        assert!(!preferences_path(tmp.path()).exists());
    }

    #[test]
    fn scrub_crash_markers_tolerates_corrupt_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prefs = preferences_path(tmp.path());
        std::fs::create_dir_all(prefs.parent().unwrap()).unwrap();
        std::fs::write(&prefs, "{ corrupt").unwrap();
        // 损坏 JSON → best-effort Ok（warn），不阻断启动；原文件不被破坏成空。
        scrub_crash_markers(tmp.path()).expect("corrupt json is best-effort benign");
        assert_eq!(std::fs::read_to_string(&prefs).unwrap(), "{ corrupt");
    }
}
