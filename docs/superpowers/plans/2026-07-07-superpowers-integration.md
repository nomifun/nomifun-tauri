# Superpowers 集成 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:executing-plans（本计划由主 agent 内联执行，逐任务 TDD + 真实 cargo 编译测试 + 频繁提交）。步骤用 `- [ ]` 跟踪。

**Goal:** 把 obra/superpowers 技能库内置进 nomifun、按周期从 GitHub Release 自动热更新、并在编码场景自动注入引导使技能自动触发。

**Architecture:** 单一物化目录（embedded baseline + 热更新 overlay，overlay 优先）→ 两条喂入（nomi 走 `AgentBootstrap::extra_skill_dirs`，ACP 走 `link_workspace_skills` → `.claude/skills`）→ 会话级编码场景判定决定是否注入 `using-superpowers` 引导。热更新是 `nomifun-extension` 内的下载/校验/原子替换模块 + `nomifun-app` 的周期 janitor。

**Tech Stack:** Rust（tokio, include_dir, zip, sha2, fs2, reqwest via nomifun-net）；工作区 crate：`nomifun-extension`、`nomifun-ai-agent`、`nomifun-app`、`nomi-agent`。

## Global Constraints
- 提交作者 = `nomifun`（`nomifun@users.noreply.github.com`），**禁止** Co-Authored-By trailer。
- 交流中文；平台 Windows x64，路径需处理 `\`/drive-letter；复用现有 `retry_startup_file_op`（容忍 os error 5/32/33）。
- 不 panic、不阻断：热更新/场景判定任何失败都 warn 并降级，绝不影响既有会话。
- superpowers 技能一律 **inline**（nomi 后端无 spawner，fork 不可用）；保留上游 `LICENSE` 与署名，不改技能正文。
- 前端若涉改用 `bun run build` 验证（本期 P1/P2 预期不涉前端）。
- 每任务：写失败测试 → 看它失败 → 最小实现 → 看它通过 → `cargo test -p <crate>` 绿 → 提交。

---

## 文件结构（新增/修改）
- 新增 `crates/backend/nomifun-extension/assets/superpowers/`：内置 superpowers 语料库（14 技能 + LICENSE + VERSION）。
- 新增 `crates/backend/nomifun-extension/src/superpowers/mod.rs`：嵌入常量、指纹、baseline 物化、有效目录解析、下载/校验/替换入口（公开 API）。
- 新增 `crates/backend/nomifun-extension/src/superpowers/update.rs`：GitHub 下载 + host 白名单 + zip 安全解压到 staging + 原子替换 overlay。
- 修改 `crates/backend/nomifun-extension/src/startup_materialize.rs`：抽出 `pub(crate)` 复用 helper（`commit_staging_dir`、`retry_startup_file_op`、`write_dir_recursive`、锁）。
- 新增 `crates/backend/nomifun-extension/src/zip_safe.rs`：从 `skill_service.rs` 提升的 `extract_zip_archive`/`safe_zip_entry_path`/`reject_zip_symlink`。
- 修改 `crates/backend/nomifun-extension/src/{lib.rs,error.rs,constants.rs,Cargo.toml}`。
- 新增 `crates/backend/nomifun-ai-agent/src/capability/superpowers_scenario.rs`：`is_coding_scenario` + 引导文本常量。
- 修改 `crates/backend/nomifun-ai-agent/src/factory/nomi.rs` + `manager/nomi/agent.rs`：编码场景→`extra_skill_dirs` + 追加引导。
- 修改 `crates/backend/nomifun-app/src/router/{routes.rs,state.rs}`：`spawn_superpowers_updater` janitor。
- 修改 `crates/backend/nomifun-ai-agent/src/capability/{prompt_pipeline 相关}`：ACP `PreSendHook` 引导（P2.5）。

---

## Phase 1 — 内置 + nomi 喂入 + 场景引导（核心，先交付）

### Task 1: 内置 superpowers 语料库 + 嵌入常量 + 指纹
**Files:** Create `assets/superpowers/**`（从本机缓存 `~/.claude/plugins/.../superpowers/6.0.3/skills` + `LICENSE` 拷入）、`src/superpowers/mod.rs`；Modify `src/lib.rs`、`Cargo.toml`(若需 include_dir 已在依赖则免)。Test: `src/superpowers/mod.rs` 内 `#[cfg(test)]`。
**Interfaces — Produces:** `pub fn superpowers_corpus() -> &'static include_dir::Dir<'static>`；`pub fn superpowers_corpus_fingerprint() -> String`（复用 `builtin_skills_corpus_fingerprint` 同算法）；`pub const SUPERPOWERS_BUNDLED_VERSION: &str`（读 `assets/superpowers/VERSION`，`include_str!`）。
- [ ] Step1 写失败测试：`corpus 含 "using-superpowers/SKILL.md" 与 "test-driven-development/SKILL.md"；fingerprint 为 64 hex 且两次调用相同`。
- [ ] Step2 `cargo test -p nomifun-extension superpowers::` → FAIL（模块不存在）。
- [ ] Step3 拷贝语料库资产；写 `mod.rs`：`static SUPERPOWERS: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/superpowers")`；`superpowers_corpus()`；`superpowers_corpus_fingerprint()`（对 `corpus` 走 sorted (path,contents) SHA-256）；`SUPERPOWERS_BUNDLED_VERSION = include_str!(".../VERSION").trim()`；`lib.rs` 加 `pub mod superpowers;`。
- [ ] Step4 测试绿。
- [ ] Step5 提交 `feat(superpowers): embed upstream skills corpus`。

### Task 2: startup_materialize 抽出复用 helper（重构，保持既有测试绿）
**Files:** Modify `src/startup_materialize.rs`。Test: 既有测试 + 新增 `commit_staging_dir` 单测。
**Interfaces — Produces:** `pub(crate) async fn commit_staging_dir(data_dir:&Path, target:&Path, staging:&Path, old:&Path) -> Result<(),ExtensionError>`（= 现 148-198 的 rename+restore 块）；`pub(crate) async fn write_dir_recursive(dir:&Dir, dest:&Path)`；`pub(crate) async fn retry_startup_file_op(...)`；`pub(crate) struct MaterializeLock { pub(crate) async fn acquire_named(data_dir:&Path, lock_name:&str) }`。
- [ ] Step1 写失败测试：`commit_staging_dir` 把 staging 内容原子搬到 target（target 预先有旧内容也能替换）。
- [ ] Step2 test → FAIL。
- [ ] Step3 重构：把 rename/restore 块提为 `commit_staging_dir`；`materialize_embedded_builtin_skills_unlocked` 改为调用它；锁改为 `acquire_named(data_dir, LOCK_FILE_NAME)`；三个 helper 提 `pub(crate)`。
- [ ] Step4 `cargo test -p nomifun-extension startup_materialize` 全绿（含既有）。
- [ ] Step5 提交 `refactor(extension): extract reusable atomic-materialize helpers`。

### Task 3: superpowers baseline 物化 + 有效目录解析
**Files:** Modify `src/superpowers/mod.rs`、`src/constants.rs`。
**Interfaces — Produces:** `pub async fn materialize_superpowers_baseline(data_dir:&Path) -> Result<bool,ExtensionError>`（→ `{data_dir}/superpowers-baseline/`，`.version` 门控 = `SUPERPOWERS_BUNDLED_VERSION`）；`pub fn effective_superpowers_dir(data_dir:&Path) -> PathBuf`（overlay `{data_dir}/superpowers/` 存在且非空则用之，否则 baseline）；常量 `SUPERPOWERS_BASELINE_DIR="superpowers-baseline"`、`SUPERPOWERS_OVERLAY_DIR="superpowers"`。
- [ ] Step1 失败测试：临时 data_dir → `materialize_superpowers_baseline` 后 baseline 含 `using-superpowers/SKILL.md`；重复调用返回 `Ok(false)`（版本门控）；无 overlay 时 `effective==baseline`，手动建非空 overlay 后 `effective==overlay`。
- [ ] Step2 FAIL。
- [ ] Step3 实现（用 Task2 helper：lock→write_dir_recursive(corpus,staging)→写 .version→commit_staging_dir；effective 判断目录存在且含至少一个子目录）。
- [ ] Step4 绿。
- [ ] Step5 提交 `feat(superpowers): materialize baseline corpus + effective dir resolution`。

### Task 4: 编码场景判定
**Files:** Create `crates/backend/nomifun-ai-agent/src/capability/superpowers_scenario.rs`；Modify `capability/mod.rs`(声明) 。
**Interfaces — Produces:** `pub struct ScenarioSignals { pub workspace: Option<PathBuf>, pub file_tools_enabled: bool, pub scenario_tags: Vec<String> }`；`pub fn is_coding_scenario(sig:&ScenarioSignals) -> bool`；`pub const SUPERPOWERS_BOOTSTRAP: &str`（nomi 语气的 using-superpowers 引导，精炼版）。
- [ ] Step1 失败测试：workspace 含 `Cargo.toml`/`package.json`→true；`file_tools_enabled=true`→true；`scenario_tags=["coding"]`→true；三者皆空/否→false。
- [ ] Step2 FAIL。
- [ ] Step3 实现纯函数（检测 workspace 下已知工程清单文件；或 file_tools_enabled；或含 coding tag）。
- [ ] Step4 绿。
- [ ] Step5 提交 `feat(superpowers): coding-scenario detector + bootstrap text`。

### Task 5: nomi 喂入 + 引导注入
**Files:** Modify `factory/nomi.rs`、`manager/nomi/agent.rs`（先精读确认 `extra_skill_dirs` 挂载点与 system_prompt 组装点）。Test: `factory/nomi.rs` 内单测（近似）。
**Interfaces — Consumes:** Task3 `effective_superpowers_dir`、Task4 `is_coding_scenario`/`SUPERPOWERS_BOOTSTRAP`、既有 `AgentBootstrap::extra_skill_dirs`。
- [ ] Step1 失败测试：给定编码场景的 resolved config，组装后的 `system_prompt` 含 `SUPERPOWERS_BOOTSTRAP` 标记串；非编码场景不含；且 bootstrap 编码场景下追加于 persona 之后（可查子串顺序）。
- [ ] Step2 FAIL。
- [ ] Step3 实现：在 system_prompt 组装链（`compose_subagent_hint` 同款位置）按场景追加引导；把 `effective_superpowers_dir(data_dir)` 经 config/overrides 传到 `NomiAgentManager`→`AgentBootstrap::extra_skill_dirs`。
- [ ] Step4 `cargo test -p nomifun-ai-agent` 相关绿。
- [ ] Step5 提交 `feat(superpowers): feed skills to nomi engine + inject bootstrap on coding scenarios`。

### Task 6: 启动物化接线
**Files:** Modify 后端启动装配处（`nomifun-app` bootstrap，与既有 builtin-skills `materialize_if_needed` 调用相邻）。
- [ ] Step1 找到既有 `materialize_if_needed` 调用点（`grep`），在其后加 `superpowers::materialize_superpowers_baseline(data_dir)`（同样先查 `NOMIFUN_BUILTIN_SKILLS_PATH` 型 env 语义，若有 superpowers 覆盖 env 则尊重）。
- [ ] Step2 `cargo build -p nomifun-app`。
- [ ] Step3 提交 `feat(superpowers): materialize baseline at startup`。

**Phase 1 收口：** `cargo build` 工作区相关 crate 通过；`cargo test -p nomifun-extension -p nomifun-ai-agent` 绿。

---

## Phase 2 — 定期自动热更新

### Task 7: 提升 zip 安全函数为共享模块
**Files:** Create `src/zip_safe.rs`；Modify `skill_service.rs`（改为引用）、`lib.rs`。
**Interfaces — Produces:** `pub(crate) fn extract_zip_archive(archive:&Path, dest:&Path) -> Result<(),ExtensionError>`；`safe_zip_entry_path`、`reject_zip_symlink`。
- [ ] Step1 迁移既有 zip 测试到 `zip_safe`（zip-slip/symlink/`..` 被拒）。
- [ ] Step2 FAIL（模块未建）。
- [ ] Step3 剪切函数到 `zip_safe.rs`，`skill_service` 改 `use crate::zip_safe::*`。
- [ ] Step4 `cargo test -p nomifun-extension` 绿（含 skill_service 既有）。
- [ ] Step5 提交 `refactor(extension): share zip-safety helpers`。

### Task 8: 错误变体 + nomifun-net 依赖
**Files:** Modify `error.rs`、`Cargo.toml`。
**Interfaces — Produces:** `ExtensionError::{Download(String), Verify(String)}`（+ `AppError` 映射）。
- [ ] Step1 失败测试：构造 `ExtensionError::Download` 显示串正确。
- [ ] Step2/3 加变体 + `Cargo.toml` 增 `nomifun-net`。
- [ ] Step4 `cargo build -p nomifun-extension` 绿。
- [ ] Step5 提交 `feat(extension): add download/verify errors + nomifun-net dep`。

### Task 9: 下载 + host 白名单 + 解压 + 原子替换 overlay
**Files:** Create `src/superpowers/update.rs`；Modify `src/superpowers/mod.rs`(re-export)。
**Interfaces — Produces:** `pub struct SuperpowersRelease { pub version:String, pub zip_url:String, pub sha256:Option<String> }`；`pub async fn install_superpowers_overlay(data_dir:&Path, http:&reqwest::Client, release:&SuperpowersRelease) -> Result<(),ExtensionError>`；`fn host_allowed(url:&str)->bool`（github.com/codeload.github.com/objects.githubusercontent.com）。
- [ ] Step1 失败测试（用本地文件 URL / 注入的 bytes，不打真网）：给定合法 zip bytes → overlay 建立且含技能；host 不在白名单→Err(Download)；坏 zip(zip-slip)→Err；sha256 不符→Err(Verify)；失败时旧 overlay 保留。（下载函数设计为可注入 `fetch: impl Fn(&str)->Result<Vec<u8>>` 以便测试，真实实现用 `http`。）
- [ ] Step2 FAIL。
- [ ] Step3 实现：host 校验→下载 bytes(超时包裹)→可选 sha256→写临时 zip→`zip_safe::extract_zip_archive` 到 staging→（release zip 有顶层目录前缀，定位到含 `skills/` 或直接技能根，规整到 staging）→写 `.version`=version→`commit_staging_dir` 到 `{data_dir}/superpowers/`。
- [ ] Step4 `cargo test -p nomifun-extension superpowers::update` 绿。
- [ ] Step5 提交 `feat(superpowers): implement GitHub release download + verified atomic overlay install`。

### Task 10: 查询最新 Release
**Files:** Modify `src/superpowers/update.rs`。
**Interfaces — Produces:** `pub async fn fetch_latest_release(http:&reqwest::Client) -> Result<SuperpowersRelease,ExtensionError>`（GET `api.github.com/repos/obra/superpowers/releases/latest`，headers Accept+User-Agent；解析 `tag_name` + `zipball_url`）。常量 `SUPERPOWERS_REPO="obra/superpowers"`（env `NOMIFUN_SUPERPOWERS_REPO` 可覆盖）。
- [ ] Step1 失败测试：给定 GitHub JSON 样例字节 → 解析出 version/zip_url（解析函数 `parse_latest_release(bytes)` 独立可测）。
- [ ] Step2/3 实现解析 + fetch。
- [ ] Step4 绿。
- [ ] Step5 提交 `feat(superpowers): fetch latest release metadata from GitHub`。

### Task 11: 周期 janitor
**Files:** Modify `nomifun-app/src/router/routes.rs`（`create_router`，事件总线→WS 桥后）、可能 `state.rs`。
**Interfaces — Consumes:** Task9/10、`nomifun_net::http_client`、`event_bus`。
- [ ] Step1 失败测试：把单次逻辑抽成 `async fn run_superpowers_update_once(data_dir, http, bus) -> bool`（返回是否更新），测其在"有新版"mock 下调用 install 并广播、"无新版"下不广播、报错不 panic。
- [ ] Step2 FAIL。
- [ ] Step3 实现 `run_superpowers_update_once` + `spawn_superpowers_updater(data_dir, bus)`（`tokio::spawn`+`interval`，env `NOMIFUN_SUPERPOWERS_AUTOUPDATE`(默认开) / `NOMIFUN_SUPERPOWERS_UPDATE_INTERVAL_SECS`(默认 21600)；比较 latest vs 当前 overlay `.version`/baseline version，semver 或字符串不等即更新）；在 `create_router` 调 spawn；广播 `WebSocketMessage::new("superpowers.updated", json!({version}))`。
- [ ] Step4 `cargo test -p nomifun-app`（相关）+ `cargo build`。
- [ ] Step5 提交 `feat(superpowers): periodic GitHub auto-update janitor`。

**Phase 2 收口：** `cargo build` + 相关 `cargo test` 绿；离线时 janitor 报错降级不崩。

---

## Phase 2.5 — ACP 路径统一（逻辑可测，端到端需真机）

### Task 12: ACP 链接 superpowers 到 .claude/skills + 引导 PreSendHook
**Files:** Modify ACP 会话构建处 + `capability/prompt_pipeline` 相关（先精读 `first_message_injector.rs`/`prompt_pipeline.rs` 确认挂点）。
**Interfaces — Consumes:** `link_workspace_skills`、`materialize_skills_for_agent`（或直接以 effective dir 下技能名）、Task4 场景判定与引导文本；新增实现 `trait PreSendHook` 的 `SuperpowersBootstrapHook`。
- [ ] Step1 失败测试：编码场景下 `SuperpowersBootstrapHook::pre_send` 在 prompt 前置引导；非编码不变。
- [ ] Step2 FAIL。
- [ ] Step3 实现 hook 并注册进 pipeline；ACP 会话构建时把 effective superpowers 技能 `link_workspace_skills(workspace, [".claude/skills"], …)`。
- [ ] Step4 `cargo test -p nomifun-ai-agent`（相关）+ build。
- [ ] Step5 提交 `feat(superpowers): ACP path — link skills + inject bootstrap`。

---

## Phase 3 — 可选增强（本次交付后按需）
- 引擎循环级 `activate_for_paths` 接入（`engine.rs:1050`），需给 `AgentEngine` 加字段/改构造器/`SkillTool` 共享可变——高危，单列。
- `SkillWatcher` 活会话热切。
- settings-UI 自动更新开关（DB 迁移 + api-types + 前端 `bun run build`）。

---

## Self-Review（对照 spec）
- **覆盖**：§4.1↔T1-3,T6；§4.2↔T5；§4.3↔T12；§4.4↔T4;§4.5↔T7-11；§4.6↔T7-8。全部 spec 需求有对应任务。
- **占位**：无 TBD；测试意图具体（含拒坏包/host/sha256/回退）。
- **类型一致**：`effective_superpowers_dir`、`is_coding_scenario`、`SUPERPOWERS_BOOTSTRAP`、`commit_staging_dir`、`install_superpowers_overlay`、`SuperpowersRelease` 在定义任务与消费任务间命名一致。
- **顺序依赖**：T2(helper)→T3(baseline)；T7(zip_safe)+T8(err/dep)→T9→T10→T11；T1→T3；T4→T5,T12。

---

## 交付状态（2026-07-07）
- ✅ **Phase 1 已交付**（T1–T6）：内置语料库 + 启动物化 + 有效目录 + 编码场景判定 + nomi `extra_skill_dirs` 喂入 + `using-superpowers` 引导注入。nomifun-ai-agent 619 / nomifun-extension 403 lib 测试全绿，nomifun-app 编译通过。
- ✅ **Phase 2 已交付**（T7–T11）：zip 安全共享化 + Download/Verify 错误 + GitHub 下载/校验/原子替换 overlay + 查询最新 Release + 周期 janitor（默认开，env 可调）。全部纯逻辑单测覆盖。
- ⏸️ **Phase 2.5（ACP）未实现，作为后续项**：让 superpowers 对 ACP（外部 Claude Code/codex）生效需跨 conversation-service 的技能解析（`AcpSkillManager` / `resolve_skill_paths` 纳入 superpowers 目录）与 ACP prompt 管线（`first_message_injector` 的 `preset_context` 追加引导）两处协同，且**无法在本环境端到端验证**（需真机外部 CLI）。接入点已定位：`capability/first_message_injector.rs` 的 `InjectionConfig.preset_context`、`skill_service::link_workspace_skills`。
- ⏸️ **Phase 3（可选增强）未实现**：引擎循环级 `ConditionalSkillManager::activate_for_paths` 接入（`engine.rs:1050`，需给 `AgentEngine` 加字段/改构造器/`SkillTool` 共享可变——高危）、`SkillWatcher` 活会话热切、settings-UI 开关（DB 迁移 + 前端 `bun run build`）。
