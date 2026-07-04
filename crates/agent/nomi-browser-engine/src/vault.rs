//! **P3-W4d：storage_state 持久化 vault（加密）+ 持久登录落盘**（DESIGN §17 / 裁决⑥ /
//! 决策1，吸收原 P6）。
//!
//! W4b/c 做的是**内存往返**（capture → [`StorageState`] → restore，绑默认 browser context）。W4d 在两端
//! 之间塞一层**磁盘 vault**：把 [`StorageState`]（cookie + localStorage 登录态）**加密**持久化到
//! per-pet workspace 的一个文件，使**跨会话/重启**的持久登录成立：
//!
//! ```text
//!   会话 A：登录 → capture_cookies/capture_local_storage → save_storage_state(vault)  [本模块]
//!                                                              ↓ 磁盘（加密）
//!   会话 B（新引擎/重启）：load_storage_state(vault)  [本模块] → EngineConfig.storage_state
//!                          → 引擎启动后 restore_cookies + restore_local_storage（灌登录态）
//! ```
//!
//! ## 为什么加密（登录态敏感）
//! storage_state 含 **cookie token / localStorage 里的 JWT** 等**等同密码的登录凭据**——明文落盘等于把
//! 账号写在磁盘上。故 vault **AES-256-GCM 加密**，**复用 [`nomifun_common`] 的
//! [`encrypt_string`](nomifun_common::encrypt_string) /
//! [`decrypt_string`](nomifun_common::decrypt_string)**（DESIGN §4/§16 裁决⑦：**不另起第二套 crypto
//! 栈**——与 `nomifun-secret` 凭据 vault 同一实现）。key = 调用方传入的 **32 字节机器绑定密钥**（app
//! data-dir 层 provision 的 `encryption_key`，全后端 `[u8; 32]` 一路穿透的同一把；本 crate **不**自造
//! 机器绑定方案，与 `nomifun-secret` 同约定）。机器绑定 key + 加密落盘 → vault 文件即便被拷走，换台机器
//! 也解不开（GCM 认证失败 → fail-closed 返 `None`）。
//!
//! ## 浏览器身份全局共享（用户决策：去 per-pet 隔离）
//! 登录态 vault 落**单一共享** [`shared_storage_state_path`]（`{data_dir}/browser-state/storage_state.enc`）
//! ——所有桌面伙伴 + 会话共享**同一份**登录态（与「多宠物统一记忆」一致），在一个伙伴里登录，另一个
//! 伙伴/会话直接是登录态。历史 per-pet workspace 落点（[`storage_state_path`]）保留：W4 引擎层 per-pet
//! context 机制**已移除**（全局共享浏览器身份）；本函数保留仅供 W4d 集成测试落 workspace vault。
//!
//! ## 优雅降级（绝不 panic）
//! [`load_storage_state`] 对**任何**读取/解密/解析失败都返 [`None`]（vault 不存在 = 首次登录、密文损坏
//! = 部分写入/磁盘坏块、key 不对 = 换机/换 key、JSON 形态变了）——持久登录是**增强**，vault 坏了应
//! **静默退回「无登录态」起点**（用户重新登录即可），绝不让一个坏 vault 文件阻断引擎启动。

use std::path::{Path, PathBuf};

use crate::storage_state::StorageState;

/// AES-256-GCM key 的字节数（与 [`nomifun_common`] / `nomifun-secret` 同 = 32）。
pub const KEY_SIZE: usize = 32;

/// per-pet workspace 下的 storage_state vault 文件名。**加密**落盘（`.enc` 后缀点明内容是密文，
/// 非明文 JSON），与 `<workspace>/downloads`（[`crate::download::DOWNLOAD_SUBDIR`]）等子产物平级隔离。
pub const STORAGE_STATE_FILE: &str = "storage_state.enc";

/// 共享 storage_state vault 的根子目录名（在 `data_dir` 下）。用户决策：浏览器身份**全局共享**——所有
/// 桌面伙伴 + 会话共享**同一份**登录态（cookie + localStorage），落
/// `{data_dir}/browser-state/storage_state.enc`。
pub const SHARED_STORAGE_STATE_DIR: &str = "browser-state";

/// 解析 per-pet workspace 的 storage_state vault 路径 `<workspace>/storage_state.enc`。
///
/// 历史 per-pet 落点（每 pet workspace 一份）。**用户决策（去 per-pet 键化）后上层不再用它**——改用
/// [`shared_storage_state_path`]（全局共享单份登录态）。本函数保留：W4 引擎层 per-pet context 机制**已移除**
/// （全局共享浏览器身份）；本函数保留仅供 W4d 集成测试落 workspace vault。
pub fn storage_state_path(workspace: &Path) -> PathBuf {
    workspace.join(STORAGE_STATE_FILE)
}

/// **单一权威：解析共享 storage_state vault 的完整路径** `{data_dir}/browser-state/storage_state.enc`。
///
/// 用户决策（去 per-pet 键化）：浏览器身份全局共享——**所有伙伴/会话的引擎实例 load/save 同一份登录态
/// vault**，故登录态（cookie + localStorage）跨伙伴/会话共享（在一个伙伴里登录，另一个伙伴/会话直接是
/// 登录态）。`data_dir` 是 app 数据目录（与 secret 的共享 vault 同源 `data_dir`）。
///
/// 安全属性不变：仍 AES-256-GCM 加密落盘（[`save_storage_state`]），坏/换 key → [`load_storage_state`]
/// fail-closed 返 `None`。共享只改「键」（单份 vs per-pet），不改加密。
pub fn shared_storage_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SHARED_STORAGE_STATE_DIR).join(STORAGE_STATE_FILE)
}

/// **[W4d] 把 storage_state 加密持久化到磁盘 vault**（持久登录的「存」侧）。
///
/// 序列化 [`StorageState`] → JSON → `nomifun_common::encrypt_string`（AES-256-GCM，机器绑定 `key`）→
/// **原子写** `vault_path`（同目录唯一临时文件 + rename 替换；同卷 rename 是原子替换）。
///
/// **为什么原子写**：per-instance profile 隔离后，多个 facade（会话/网关每 key/编排节点）会**并发**把
/// 登录态存进同一个共享 vault。直接 `write` 可能被并发写者撕裂成半截密文；虽然 load 侧 GCM 认证失败会
/// 兜底成 `None`（丢一轮登录、不崩），但原子 temp+rename 让读者**永远只看到完整文件**（末位写者胜），
/// 消除撕裂。临时名带 pid+纳秒，故并发写者各用各的临时文件、互不覆盖。
/// 父目录不存在则 best-effort 建（per-pet workspace 可能首次落 vault）。
///
/// `key` 必须恰 32 字节（[`KEY_SIZE`]），否则 [`VaultError::Crypto`]。失败 → `Err`（**绝不 panic**）；
/// 调用方（上层接线 teardown/按需 save）记录 warn 即可，存失败不应致命（下次再存）。
pub fn save_storage_state(
    state: &StorageState,
    vault_path: &Path,
    key: &[u8],
) -> Result<(), VaultError> {
    let json = serde_json::to_string(state).map_err(|e| VaultError::Serialize(e.to_string()))?;
    let ciphertext = nomifun_common::encrypt_string(&json, key)
        .map_err(|e| VaultError::Crypto(e.to_string()))?;
    if let Some(parent) = vault_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(VaultError::Io(format!(
            "create vault parent dir {}: {e}",
            parent.display()
        )));
    }
    // 原子写：同目录唯一临时文件（pid+纳秒 → 并发写者互不覆盖）→ rename 替换（同卷原子）。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = vault_path.with_extension(format!("tmp-{}-{}", std::process::id(), nanos));
    if let Err(e) = std::fs::write(&tmp, ciphertext.as_bytes()) {
        return Err(VaultError::Io(format!("write vault tmp {}: {e}", tmp.display())));
    }
    if let Err(e) = std::fs::rename(&tmp, vault_path) {
        let _ = std::fs::remove_file(&tmp); // 清理未落地的临时文件（best-effort）
        return Err(VaultError::Io(format!("rename vault {}: {e}", vault_path.display())));
    }
    Ok(())
}

/// **[W4d] 从磁盘 vault 解密读出 storage_state**（持久登录的「取」侧）。
///
/// 读 `vault_path` → `nomifun_common::decrypt_string`（机器绑定 `key`，GCM 认证）→
/// [`StorageState::from_json`]。**任何失败都返 [`None`]（绝不 panic / 绝不 `Err`）**——持久登录是增强，
/// vault 缺失/损坏/key 不对/JSON 变形都应**优雅退回「无登录态」起点**：
/// - vault 文件不存在（首次登录前）→ `None`；
/// - 读文件 I/O 失败 → `None`（warn 留痕）；
/// - base64/密文损坏或 key 不对（换机/换 key/部分写入）→ GCM 认证失败 → `None`；
/// - 解密出的串非合法 storage_state JSON（旧格式/被篡改）→ `None`。
///
/// 这把「坏 vault 阻断启动」的风险彻底消除：最坏情况是丢登录态（用户重登），不是崩。
pub fn load_storage_state(vault_path: &Path, key: &[u8]) -> Option<StorageState> {
    // vault 不存在 = 首次登录前的正常态，连 warn 都不必（Ok(None) 路径）。
    let ciphertext = match std::fs::read_to_string(vault_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                target: "nomi_browser_engine::vault",
                error = %e, path = %vault_path.display(),
                "read storage_state vault failed; falling back to no persisted login"
            );
            return None;
        }
    };
    let plaintext = match nomifun_common::decrypt_string(&ciphertext, key) {
        Ok(p) => p,
        Err(e) => {
            // 密文损坏 / key 不对（换机/换 key）→ GCM 认证失败。fail-closed 退回无登录态。
            tracing::warn!(
                target: "nomi_browser_engine::vault",
                error = %e, path = %vault_path.display(),
                "decrypt storage_state vault failed (corrupt or wrong key); falling back to no persisted login"
            );
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&plaintext) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "nomi_browser_engine::vault",
                error = %e, path = %vault_path.display(),
                "parse decrypted storage_state vault as JSON failed; falling back to no persisted login"
            );
            return None;
        }
    };
    match StorageState::from_json(value) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(
                target: "nomi_browser_engine::vault",
                error = %e, path = %vault_path.display(),
                "decrypted storage_state JSON is not a valid StorageState; falling back to no persisted login"
            );
            None
        }
    }
}

/// storage_state vault 的**存**侧错误（save 用）。**取**侧（[`load_storage_state`]）不用错误——一律
/// `None`（fail-closed 优雅降级，见其 doc）。
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// 序列化 [`StorageState`] → JSON 失败（理论上不会——本结构全可序列化）。
    #[error("serialize storage_state failed: {0}")]
    Serialize(String),
    /// AES-256-GCM 加密失败（多为 key 非 32 字节）。
    #[error("encrypt storage_state vault failed: {0}")]
    Crypto(String),
    /// vault 文件 I/O 失败（建目录 / 写文件）。
    #[error("storage_state vault I/O failed: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_state::{OriginStorage, StorageState, StorageStateCookie};

    /// 测试用 32 字节 key（机器绑定 key 的占位）。
    fn test_key() -> [u8; KEY_SIZE] {
        [0x42; KEY_SIZE]
    }

    /// 造一份**全字段**的 storage_state（cookie 含 CHIPS partitionKey + localStorage 多 origin），
    /// 覆盖 W4d vault 必须保真的全部登录态字段。
    fn full_state() -> StorageState {
        StorageState {
            cookies: vec![StorageStateCookie {
                name: "session_id".into(),
                value: "deadbeef-secret-token".into(),
                domain: ".example.com".into(),
                path: "/app".into(),
                expires: 1_900_000_000.0,
                http_only: true,
                secure: true,
                session: false,
                same_site: Some(crate::storage_state::SameSite::None),
                priority: crate::storage_state::Priority::High,
                source_scheme: crate::storage_state::SourceScheme::Secure,
                source_port: 443,
                partition_key: Some(crate::storage_state::PartitionKey {
                    top_level_site: "https://embedder.example".into(),
                    has_cross_site_ancestor: true,
                }),
            }],
            local_storage: vec![
                OriginStorage::new_local_storage(
                    "https://app.example.com",
                    [
                        ("auth_token".to_string(), "jwt.eyJ.signature".to_string()),
                        ("note".to_string(), "a=b&c={\"x\":1}".to_string()),
                    ],
                ),
                OriginStorage::new_local_storage(
                    "http://localhost:8080",
                    [("dev_flag".to_string(), "on".to_string())],
                ),
            ],
        }
    }

    #[test]
    fn save_then_load_round_trips_full_state() {
        // **核心：存→取往返保真**——save 加密落盘 → load 解密读回 == 原 storage_state（全字段不丢）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        let original = full_state();
        save_storage_state(&original, &path, &test_key()).expect("save");
        let loaded = load_storage_state(&path, &test_key()).expect("load Some");
        assert_eq!(loaded, original, "vault round-trip must preserve full storage_state");
        // 显式钉死最易丢的 CHIPS partitionKey 经 vault 往返不丢。
        let pk = loaded.cookies[0].partition_key.as_ref().expect("partitionKey survives vault");
        assert_eq!(pk.top_level_site, "https://embedder.example");
        assert!(pk.has_cross_site_ancestor);
        // localStorage 多 origin 经 vault 往返不丢。
        assert_eq!(loaded.local_storage.len(), 2);
    }

    #[test]
    fn vault_file_is_ciphertext_not_plaintext() {
        // **加密验收：落盘内容是密文，不含明文登录态**（cookie token / JWT 不得明文上盘）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        save_storage_state(&full_state(), &path, &test_key()).expect("save");
        let on_disk = std::fs::read_to_string(&path).expect("read raw vault");
        assert!(!on_disk.contains("deadbeef-secret-token"), "cookie token must NOT be plaintext on disk");
        assert!(!on_disk.contains("jwt.eyJ.signature"), "localStorage JWT must NOT be plaintext on disk");
        assert!(!on_disk.contains("session_id"), "even cookie name must NOT be plaintext (whole JSON encrypted)");
        // 文件应是 base64 密文（nomifun_common::encrypt_string 产物）——非空、可解回。
        assert!(!on_disk.is_empty());
        assert!(load_storage_state(&path, &test_key()).is_some(), "ciphertext must decrypt back");
    }

    #[test]
    fn vault_path_is_under_workspace() {
        // per-pet workspace 落点（机制已移除，函数保留供集成测试）：vault 落 <workspace>/storage_state.enc（与 download_dir 同范式）。
        let ws = Path::new("/data/companion-xyz/workspace");
        let p = storage_state_path(ws);
        assert_eq!(p, Path::new("/data/companion-xyz/workspace/storage_state.enc"));
        assert!(p.starts_with(ws), "vault must live under the per-pet workspace");
    }

    #[test]
    fn shared_storage_state_path_is_single_under_data_dir() {
        // 用户决策（去 per-pet 键化）：所有伙伴/会话共享**同一份**登录态 vault
        // `{data_dir}/browser-state/storage_state.enc`——与 data_dir 同源，不含 pet/workspace 段。
        let data = Path::new("/data");
        let p = shared_storage_state_path(data);
        assert_eq!(p, Path::new("/data/browser-state/storage_state.enc"));
        // 共享单例：data_dir 固定 → 路径固定（不随伙伴/会话变），故 save/load 命中同一文件 → 登录态共享。
        assert_eq!(shared_storage_state_path(data), shared_storage_state_path(data));
        // 与 per-pet workspace 落点不同根（共享在 data_dir 下，per-pet 在各自 workspace 下）。
        assert_ne!(p, storage_state_path(Path::new("/data/companion-a/workspace")));
    }

    #[test]
    fn shared_vault_round_trips_login_across_engines() {
        // **共享证据（纯逻辑）**：引擎实例 A（伙伴 A）save 登录态到共享路径 → 引擎实例 B（伙伴 B，同
        // data_dir）load 同一共享路径 → 拿回 A 的登录态（cookie + localStorage 跨伙伴共享）。
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path();
        let shared = shared_storage_state_path(data);

        // 伙伴 A 登录后 save（加密落共享 vault）。
        save_storage_state(&full_state(), &shared, &test_key()).expect("A save shared");
        // 伙伴 B（不同伙伴，同 data_dir）解析同一共享路径 → load 拿回 A 的登录态。
        assert_eq!(shared_storage_state_path(data), shared, "B resolves the same shared vault");
        let loaded = load_storage_state(&shared, &test_key()).expect("B load shared Some");
        assert_eq!(loaded, full_state(), "login state registered by A must be visible to B (shared identity)");
        // 安全不变：换 key 仍 fail-closed（共享只改键不改加密）。
        assert!(load_storage_state(&shared, &[0x99; KEY_SIZE]).is_none(), "wrong key still fail-closed");
    }

    #[test]
    fn load_missing_vault_is_none_not_panic() {
        // 优雅：vault 文件不存在（首次登录前）→ None（绝不 panic）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path()); // 没 save 过，文件不存在
        assert!(load_storage_state(&path, &test_key()).is_none());
    }

    #[test]
    fn load_corrupt_ciphertext_is_none_not_panic() {
        // 优雅：密文损坏（部分写入/磁盘坏块/被改）→ GCM 认证失败 → None（绝不 panic）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        std::fs::write(&path, "this-is-not-valid-base64-ciphertext!!!").expect("write garbage");
        assert!(load_storage_state(&path, &test_key()).is_none(), "corrupt vault must degrade to None");
    }

    #[test]
    fn load_with_wrong_key_is_none_fail_closed() {
        // 优雅 + 安全：换机/换 key → GCM 认证失败 → None（vault 拷走也解不开，fail-closed）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        save_storage_state(&full_state(), &path, &[0x01; KEY_SIZE]).expect("save with key1");
        // 另一把 key 解 → None（绝不返回乱码 StorageState）。
        assert!(load_storage_state(&path, &[0x02; KEY_SIZE]).is_none(), "wrong key must fail-closed to None");
        // 对的 key 仍能解回（证明只是 key 不对，不是 vault 坏）。
        assert!(load_storage_state(&path, &[0x01; KEY_SIZE]).is_some());
    }

    #[test]
    fn load_valid_ciphertext_of_non_storage_state_json_is_none() {
        // 优雅：用对的 key 加密了一段**不是 StorageState**的 JSON（旧格式/异类）→ 解得开但解析失败 → None。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        // StorageState::from_json 走 serde_value→struct；非对象（数组/标量）必失败。
        let bogus = nomifun_common::encrypt_string("[1,2,3]", &test_key()).expect("encrypt bogus");
        std::fs::write(&path, bogus.as_bytes()).expect("write");
        assert!(load_storage_state(&path, &test_key()).is_none(), "non-StorageState JSON must degrade to None");
    }

    #[test]
    fn save_empty_state_round_trips() {
        // 空 storage_state（无 cookie 无 localStorage）也能存取（首次 save 占位 / 登出后清空）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        let empty = StorageState::default();
        save_storage_state(&empty, &path, &test_key()).expect("save empty");
        let loaded = load_storage_state(&path, &test_key()).expect("load empty Some");
        assert_eq!(loaded, empty);
        assert!(loaded.cookies.is_empty());
        assert!(loaded.local_storage.is_empty());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        // 原子写：save 后目录里只有 vault 本身，无残留临时文件（temp+rename 已完成替换）。
        // 再存一次（覆盖）仍只剩 vault，且可正常读回（末位写者胜）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        save_storage_state(&full_state(), &path, &test_key()).expect("save");
        save_storage_state(&full_state(), &path, &test_key()).expect("overwrite save");

        let entries: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let vault_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            entries,
            vec![vault_name.clone()],
            "after save the dir must hold only the vault (no leftover .tmp-* files): {entries:?}"
        );
        assert!(load_storage_state(&path, &test_key()).is_some(), "vault loads after atomic save");
    }

    #[test]
    fn save_creates_parent_dir() {
        // per-pet workspace 可能首次落 vault（父目录还没建）→ save best-effort 建父目录。
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("companion-new").join("workspace");
        let path = storage_state_path(&nested);
        assert!(!nested.exists(), "precondition: parent not yet created");
        save_storage_state(&full_state(), &path, &test_key()).expect("save into not-yet-existing dir");
        assert!(path.exists(), "save must create parent dirs");
        assert!(load_storage_state(&path, &test_key()).is_some());
    }

    #[test]
    fn save_with_bad_key_size_errors_not_panic() {
        // key 非 32 字节 → VaultError::Crypto（绝不 panic）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());
        let err = save_storage_state(&full_state(), &path, &[0u8; 16]).expect_err("short key must error");
        assert!(matches!(err, VaultError::Crypto(_)), "short key → Crypto error, got {err:?}");
    }

    #[test]
    fn vault_roundtrip_includes_indexeddb() {
        // **核心**：IndexedDB dump 经 vault 加密→落盘→解密→读回全字段保真（含 base64 二进制哨兵）。
        use crate::storage_state::{
            IdbDatabase, IdbStore, IndexedDbDump, encode_binary_sentinel,
        };

        let state_with_idb = StorageState {
            cookies: vec![StorageStateCookie {
                name: "sid".into(),
                value: "tok123".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                expires: 2_000_000_000.0,
                http_only: true,
                secure: true,
                session: false,
                same_site: Some(crate::storage_state::SameSite::Lax),
                priority: crate::storage_state::Priority::Medium,
                source_scheme: crate::storage_state::SourceScheme::Secure,
                source_port: 443,
                partition_key: None,
            }],
            local_storage: vec![OriginStorage {
                origin: "https://app.example.com".into(),
                local_storage: vec![crate::storage_state::LocalStorageItem {
                    name: "theme".into(),
                    value: "dark".into(),
                }],
                index_db: Some(IndexedDbDump {
                    databases: vec![IdbDatabase {
                        name: "appdb".into(),
                        version: 5,
                        stores: vec![
                            IdbStore {
                                name: "cache".into(),
                                key_path: Some("url".into()),
                                auto_increment: false,
                                records: vec![
                                    serde_json::json!({"url": "/api/v1", "data": "cached"}),
                                    serde_json::json!({"url": "/img/logo", "blob": encode_binary_sentinel(&[0xDE, 0xAD, 0xBE, 0xEF])}),
                                ],
                            },
                            IdbStore {
                                name: "queue".into(),
                                key_path: None,
                                auto_increment: true,
                                records: vec![
                                    serde_json::json!("task1"),
                                    serde_json::json!(42),
                                ],
                            },
                        ],
                    }],
                }),
            }],
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = storage_state_path(dir.path());

        // Save (encrypt + write).
        save_storage_state(&state_with_idb, &path, &test_key()).expect("save with IDB");

        // Verify ciphertext doesn't contain plaintext IDB data.
        let on_disk = std::fs::read_to_string(&path).expect("read raw vault");
        assert!(
            !on_disk.contains("appdb"),
            "IDB database name must NOT be plaintext on disk"
        );
        assert!(
            !on_disk.contains("__b64__"),
            "base64 sentinel must NOT be plaintext on disk"
        );

        // Load (decrypt + parse).
        let loaded = load_storage_state(&path, &test_key()).expect("load with IDB");
        assert_eq!(
            loaded, state_with_idb,
            "vault must round-trip StorageState with IndexedDB intact"
        );

        // Explicit assertions on the IDB dump after vault round-trip.
        let idb = loaded.local_storage[0]
            .index_db
            .as_ref()
            .expect("index_db must survive vault round-trip");
        assert_eq!(idb.databases.len(), 1);
        let db = &idb.databases[0];
        assert_eq!(db.name, "appdb");
        assert_eq!(db.version, 5);
        assert_eq!(db.stores.len(), 2);

        // Verify the base64 binary sentinel survives vault (no data corruption).
        let cache_store = db.stores.iter().find(|s| s.name == "cache").expect("cache");
        let rec2 = &cache_store.records[1];
        let blob_field = rec2.get("blob").expect("blob field");
        let decoded = crate::storage_state::decode_binary_sentinel(blob_field)
            .expect("decode base64 sentinel after vault");
        assert_eq!(decoded, vec![0xDE, 0xAD, 0xBE, 0xEF], "binary must survive vault");

        // Verify out-of-line key store survives.
        let queue_store = db.stores.iter().find(|s| s.name == "queue").expect("queue");
        assert!(queue_store.key_path.is_none());
        assert!(queue_store.auto_increment);
        assert_eq!(queue_store.records.len(), 2);
    }
}
