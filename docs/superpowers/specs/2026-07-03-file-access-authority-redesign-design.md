# 文件访问权限重设计:随会话信任分级的统一模型

**日期**: 2026-07-03
**状态**: 已批准(方案 1 + 方案 3 UX),直接实施
**分支**: `feat/file-access-authority-redesign`

## 背景与问题

用户反馈:**临时会话中 Agent 无法修改其它路径上的文件**。现场截图:临时会话(workspace=`nomi-temp-5`),Agent 报"当前环境的沙箱限制只能访问工作区目录,无法直接访问 `C:\code…`"。

根因排查(见 memory `temp-session-path-write-investigation`)结论:文件访问权限被**劈成两套互不一致、且都不看会话信任等级**的机制:

1. **原生 nomi-tools**(Read/Write/Edit/ApplyPatch/Bash):`write_root` 默认 `None` → 完全不受限;后端从不设置。绝对路径全放行。**不区分 Desktop/Channel/Remote。**
2. **gateway file-service**(`nomi_fs_*` + UI 文件面板):被全局固定的 `allowed_roots = [temp_dir, home_dir, work_dir, data_dir]` 钳制(`nomifun-file` 单例,构造期定死),**不含会话工作区、也不含用户随口指定的 `C:\code`**;`extra_root` 只能"加宽",无法"收窄"或"放开到 OS 全权"。**同样不区分 surface。**

Agent 在桌面会话里用了 `nomi_fs_*`(第二通道),撞上 `allowed_roots` 的 `Forbidden`,然后过度概括成"只能访问工作区",且未回退到不受限的原生工具。

**核心矛盾**:`Desktop` 需要"更宽(OS 全权)",`Channel/Remote` 需要"更窄(仅工作区)"。现有的 `base_roots ∪ extra_root` 模型两个方向都表达不了。

## 目标

把文件访问权限收敛为**"随会话信任分级"的单一模型**,一致地施加到两条通道:

| Surface | 文件权限 | 破坏性操作 |
|---|---|---|
| **Desktop**(可信本地,机主自己) | **不限(OS 用户全权)** | 仍二次确认(现有 DangerTier 矩阵不变) |
| **Channel**(IM 陌生人) | 限会话工作区;destructive/sensitive 已 Deny | 不变 |
| **Remote**(外部/LAN) | 限会话工作区;sensitive Deny | destructive 确认 |
| **Public**(对外伙伴) | 工具不注册(exposure clamp,已存在) | — |

安全前提:Desktop = 机主本人 + 本机 agent,OS 账户权限即天然边界;把 agent 钳在人造 `allowed_roots` 内既挡不住恶意(它还有原生工具/Bash),又只会误伤正常使用。对外面(Channel/Remote/Public)保持并强化收窄。

## 设计

### 核心抽象:`PathAuthority`(新增于 `nomifun-file::path_safety`)

```rust
pub enum PathAuthority {
    /// OS 用户全权:跳过 root 包含校验(traversal/NUL 仍校验)。Desktop 机主。
    Unrestricted,
    /// 必须落在这些 root 之一内(现有 allowed_roots 语义)。对外面 + UI。
    Confined(Vec<PathBuf>),
}
```

- 新增 `validate_path_for_write_with_authority(path, &PathAuthority)` / 读版,内部:先做 `has_traversal`/NUL 预拒(两档都做),再按 authority 分支:`Unrestricted` → 仅规范化返回;`Confined(roots)` → 现有 `starts_with(canonical_root)` 校验。

### 单元 1:`nomifun-file` — file-service 接受 authority

**接口取舍**:把路径作用域族方法的 `extra_root: Option<&Path>` / 依赖 `allowed_roots` 的校验改为显式接收 `authority: &PathAuthority`。涉及 `read_file` / `read_file_buffer` / `get_file_metadata` / `get_image_base64`(读族)与 `write_file` / `remove_entry` / `get_files_by_dir` / `list_workspace_files` / `rename_entry`(写/浏览族)。

- `FileService` 仍持有基础 `allowed_roots` 作为 `Confined` 默认集。
- 现有 UI/内部调用者传 `Confined(allowed_roots ∪ 请求 workspace)` = **今日行为等价**(零回归)。
- 依赖:`nomifun-common`(AppError)。测试:Unrestricted 放行任意盘符;Confined 拒 root 外;traversal 两档都拒。

### 单元 2:`nomifun-gateway` — 按 surface 解析 authority

`caps_files.rs` 各 handler 用 `ctx.surface()`:
- `Desktop` → `PathAuthority::Unrestricted`。
- `Channel`/`Remote` → `PathAuthority::Confined([workspace])`(取请求里的 workspace/root;为空则回退基础 allowed_roots)。

调用改为传 authority。`nomi_fs_read_file` 现在硬传 `None` → 改为 authority。写/删的 `workspace` 参数保留用于事件 scoping。DangerTier 矩阵/`deny_on` 不动(destructive/sensitive 在 Channel 仍 Deny)。

- 依赖:`nomifun-file::PathAuthority`。测试:Desktop caller 放行 `C:\code`;Channel caller 拒工作区外。

### 单元 3:`nomifun-ai-agent` — 原生工具 write_root 按 surface

- `NomiResolvedConfig` 新增 `write_root: Option<String>`。
- `factory/nomi.rs`:据 `overrides.exposure`/`channel_platform`/`remote`(与 gateway surface 同源的纯函数 `resolve_file_authority_surface`)解析:Desktop → `None`;Channel/Remote → `Some(workspace)`。Public 已 clamp。
- `manager/nomi/agent.rs`:把它写进 `config.tools.write_root`(在现有 override 段)。
- 净效果:Desktop 原生工具维持不限(今日行为);Channel/Remote 原生写收窄到工作区(**顺带修掉原生工具对对外面过度开放的隐患**)。
- 依赖:纯 surface 判定函数(可单测)。

### 单元 4:方案 3 UX — 会话内绑定/切换真实工作目录

**后端**(`nomifun-conversation`):
- 确认 PATCH `/api/conversations/{id}` 合并 `extra.workspace` 可用(已存在)。
- **补缺口**:`update()` 现在只在 model 变更时 kill agent(`service.rs:965`);增加"workspace 变更也 kill/重建 agent",使新 cwd 立即生效(不变量:mid-turn 运行中不 kill,延后到下条消息,与 knowledge 绑定变更同策略)。
- 校验复用 `normalize_workspace_path`;对绑定目录无 data_dir 归属限制(允许指向任意真实目录)。

**前端**(`ui`):
- 在会话工作区面板头部(`WorkspacePanelHeader` / `WorkspaceOpenButton` 区域)为**临时会话**新增"设置工作目录"入口:`ipcBridge.dialog.showOpen({openDirectory,createDirectory})` → PATCH 会话 `extra.workspace` → 刷新会话。
- 绑定后:该会话不再是临时会话(workspace 移出 data_dir → `is_temporary_workspace` 自动变 false),"临时空间"只读标签变为真实路径,`WorkspaceOpenButton` 重新出现。
- UX 文案:明确"绑定后 Agent 将以此目录为主工作区"。
- i18n:`conversation.workspace.*` 补键(root `bun run gen:i18n`)。

## 数据流(修复后)

用户在临时会话让 Agent 改 `C:\code\x`:
- 若走原生 Write:`write_root=None`(Desktop)→ 放行(今日即可)。
- 若走 `nomi_fs_write_file`:`ctx.surface()=Desktop` → `PathAuthority::Unrestricted` → file-service 放行。**不再 Forbidden。**
- Channel 陌生人让改 `C:\code\x`:`Confined([workspace])` → 拒(安全保持)。

## 错误处理
- `Confined` 拒绝时 `AppError::Forbidden`,文案改为可自纠:说明"目标在允许范围外",不误导为"只能访问工作区"。
- workspace 绑定非法路径:`normalize_workspace_path` 现有校验(空/首尾空白段)返回 `BadRequest`。

## 测试策略
- `nomifun-file`:path_safety authority 单测(Unrestricted/Confined/traversal)。
- `nomifun-gateway`:caps_files surface→authority 映射单测。
- `nomifun-ai-agent`:`resolve_file_authority_surface` 纯函数单测 + write_root 装配。
- `nomifun-conversation`:workspace 变更触发 agent 重建的服务测试。
- 前端:typecheck 0;无可跑 vitest(见 memory `frontend-test-harness-reality`)。
- 收尾:触碰 crate `cargo nextest`;`cargo clippy` 零告警。

## 非目标(YAGNI)
- 不做方案 2 的"删除冗余 nomi_fs_* 通道"(authority 一致后双通道已安全共存;确认无依赖再另议)。
- 不动 UI 文件面板对普通会话的现有 allowed_roots 行为(非本 bug)。
- 不做"已授权目录集合持久记忆"(方案 3 的更重变体)。

## 关联
memory: `temp-session-path-write-investigation`、`per-companion-capabilities-delivered`、`external-capability-exposure-p0-delivered`、`remote-gateway-redesign`。
