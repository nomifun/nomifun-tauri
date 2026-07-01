# Browser 使用模式：静默默认 + 来源可选（+ 未来内置浏览器）

- 日期：2026-07-01
- 分支：`feat/browser-modes-silent-and-system`
- 状态：设计已确认（用户逐项拍板），实现分期进行

## 1. 背景与目标

现状：`browser-use` 由自研的进程内 Rust CDP 引擎 `nomi-browser-engine`（仅 Chromium）实现。它**总是自己拉起一个独立的 Chromium 子进程**，用**专属 `--user-data-dir`**（红线：绝不碰用户真实 Chrome 资料目录），通过 CDP 管道连接。在桌面端，因为 `BrowserConfig.headless` 默认 `false`，引擎会**弹出一个可见窗口** —— 这正是用户的痛点：弹窗浏览器既不是用户常用的浏览器，也打扰体验。

用户诉求（原话概括）：
1. **静默浏览器**（用户看不到窗口，系统直接驱动）—— 高优
2. **驱动用户常用浏览器**（登录态天然就有、操作丝滑）—— 高优
3. **app 内置浏览器**（一站式）—— 低优

经澄清，诉求 2 的真实语义是 **「用我系统里装的 Chrome/Edge 本体 + 一个独立持久 profile」**（不是接管正在运行的那个 Chrome 窗口 —— 那受 Chrome 136+ 安全限制且需重写连接层）。登录态复用现有的**持久登录保险库**。

用户明确要求：**同时支持多种模式、默认静默、用户可自由切换。**

### 目标（本设计交付）
把上面两个高优诉求落成**两个正交的、用户可切换的开关**,默认静默:

- **可见性**：`agent.browserUse.silent`（默认 **ON = 静默/headless**；OFF = 弹出可见窗口）
- **浏览器来源**：`agent.browserUse.source`（默认 `managed` = 内置/下载的 Chrome for Testing；`system` = 用户系统安装的 Chrome/Edge 本体，仍是独立 profile）

### 非目标
- **不**接管用户正在运行的浏览器 / 不使用用户真实 profile（红线保持）。
- **不**新增「连接到外部 CDP endpoint」的传输层。
- app 内置浏览器（诉求 3）本期不做，仅在 §7 记录后续路径。

## 2. 现状关键事实（实现依据，含文件/行）

- 引擎入口 `nomi_browser_engine::create_engine(EngineConfig)`（`crates/agent/nomi-browser-engine/src/lib.rs:203`）：resolve chrome → 专属 `user_data_dir=<data_dir>/profile`（`lib.rs:209`）→ `build_backend` 启动。
- Chrome 解析 `acquire::resolve_chrome_path`（`acquire.rs:236`）优先级：`NOMIFUN_CHROME_BINARY` env > 打包 CfT > 已下载 CfT > **系统 Chrome/Edge**（`detect_system_browser_in`，`acquire.rs:215`）> 下载 CfT。**系统浏览器探测逻辑已存在**，只是排在 CfT 之后当兜底。
- headless 决策 `backend/cdp.rs::build_backend`：`force_headless = !display_available() || !config.headful`。`EngineConfig.headful` 来自 `BrowserConfig.headless`(`!headless`)。`--headless=new` 已是现成机制（截图可用）。
- `BrowserConfig`（`crates/agent/nomi-config/src/config.rs:283`）有 `headless: bool`（默认 false），**无 UI 开关**（仅 config.toml）。
- 配置链路：UI `configService` → `client_preferences` → 后端工厂 `factory/nomi.rs` 每会话 `read_bool_pref` LIVE 读 → `NomiResolvedConfig`（`types.rs:59`）→ `manager/nomi/agent.rs:235-249` 写进 `config.tools.browser.*` → `bootstrap.rs:670` `BrowserTool::with_policy` → `tool.rs::engine()`(`tool.rs:773`) 构造 `EngineConfig`。
- `BrowserTool`（`tool.rs`）在 `new()`(`tool.rs:359`) 由 `!config.headless` 得 `headful` 存字段；`with_policy` **不带** headful/source 参 —— 故这两个开关都经 `config.tools.browser.*` 流入，**无需改 `with_policy` 签名**。
- 唯一浏览器设置 UI：`ui/.../BrowserUseSettingsContent.tsx`（6 个 Arco `Switch`，无来源/静默控件）。
- 前端 schema：`ui/src/common/config/configKeys.ts:63-87`（`agent.browserUse.*`）。
- 另两个引擎构造点不受本设计影响：`browser_fetcher.rs`（知识抓取，恒 headless，`..EngineConfig::default()`）、`browser_stdio.rs`（MCP 桥，`BrowserConfig::default()`）。

## 3. 三处已确认的取舍（均取方案 A）

1. **切换何时生效**：下一次浏览器会话生效（贴合「每会话读 LIVE pref」现有架构；不打断进行中的任务）。Chrome 无法对运行中进程改 headless 或换二进制，故不做「立即重启当前浏览器」。
2. **`system` 来源的登录态**：复用现有**持久登录保险库**（`persistentLogin`，默认 ON）。守住「不碰真实 profile」红线。可选的「登录」按钮见 §6（Phase 2）。
3. **静默下的高风险审批（takeover）**：需要人工确认时在 app 内以「截图 + 批准/拒绝」呈现，不依赖弹出真实窗口（Phase 3）。

## 4. 设计：两个正交开关

### 4.1 数据流（复用现有范式，零新范式）

```
UI 两个控件 (Switch + Radio.Group)
  → configService 写 client_preferences:
      agent.browserUse.silent  (bool,   缺省=ON=静默)
      agent.browserUse.source  (string, 'managed'|'system', 缺省='managed')
  → factory/nomi.rs 每会话 LIVE 读:
      read_bool_pref(PREF_BROWSER_SILENT, host_default=true)
      read_string_pref(PREF_BROWSER_SOURCE, host_default="managed")   // 新增 helper
  → NomiResolvedConfig { browser_silent: bool, browser_source: String }  // 新增两字段
  → manager/nomi/agent.rs 写进 config.tools.browser:
      config.tools.browser.headless = browser_silent      // silent → headless
      config.tools.browser.source   = browser_source       // 新增 String 字段
  → BrowserTool::new(&config): headful = !config.headless（现有）
                               chrome_source = ChromeSource::from_source_str(&config.source)（新增）
  → tool.rs::engine() → EngineConfig { headful, chrome_source, .. }
  → create_engine → resolve_chrome_path_with_source(.., chrome_source)
```

**默认静默的兑现**：`silent` 的 host_default = `true` → `headless=true` → `headful=false` → 引擎 headless。老用户从未设置过该 pref，读到缺省即静默 —— 直接消除弹窗（正是用户诉求）。config.toml 的 `headless`（默认 false）仍保留给 CLI/高级用户;桌面会话由 pref 覆写(pref 优先)。

### 4.2 引擎层改动（`nomi-browser-engine`）

**`acquire.rs`**：
- 新增 `pub enum ChromeSource { Managed, System }`（`Default = Managed`, `Copy`），带 `from_source_str(&str)`（`"system"`→System，其余→Managed）。
- 重构（纯逻辑、可注入 `exists` 单测）：拆出 `env_chrome_path` / `cft_chrome_path`，新增纯函数
  `resolve_local_chrome(platform, os, source, env_get, exists, bundled_dir, data_dir) -> Option<PathBuf>`：
  - `env` 覆写在两种 source 下都最高优先。
  - `System`：系统 Chrome/Edge 优先，未找到回退 CfT（bundled/downloaded）。
  - `Managed`：CfT 优先，未找到回退系统浏览器（保持现行为）。
- 新增 `pub async fn resolve_chrome_path_with_source(data_dir, bundled_dir, source)`；保留
  `resolve_chrome_path(data_dir, bundled_dir)` 委托 `Managed`（**不改现有 ~10 个调用点**）。
- 新增单测覆盖 source 排序 + env 覆写 + 回退。

**`lib.rs`**：
- `pub use acquire::ChromeSource;`
- `EngineConfig` 加 `pub chrome_source: ChromeSource`（Debug/Default=Managed）。
- `create_engine` 用 `resolve_chrome_path_with_source(.., config.chrome_source)`。
- 更新 `EngineConfig` 字面量构造点（`lib.rs` 测试 + `tool.rs:773`）与默认断言。

### 4.3 工具/配置/工厂/管理器改动

- `nomi-browser/src/tool.rs`：`BrowserTool` 加 `chrome_source: ChromeSource` 字段；`with_data_dir` 默认 `Managed`；`new()` 从 `config.source` 解析；`engine()` 的 `EngineConfig` 字面量加 `chrome_source: self.chrome_source`。**不动 `with_policy` 签名。**
- `nomi-config/src/config.rs`：`BrowserConfig` 加 `source: String`（`#[serde(default = "default_browser_source")]`，默认 `"managed"`）；`Default` 与 layer-merge 补上（project 非默认则覆盖 global）。
- `types.rs`：`NomiResolvedConfig` 加 `browser_silent: bool` + `browser_source: String`。
- `factory/nomi.rs`：加 `PREF_BROWSER_SILENT="agent.browserUse.silent"` / `PREF_BROWSER_SOURCE="agent.browserUse.source"`；`browser_silent`=`read_bool_pref(..,true)`；`browser_source`=`read_string_pref(..,"managed")`（新增 `read_string_pref` helper）；填入 `NomiResolvedConfig`。
- `manager/nomi/agent.rs`：在现有 browser 字段映射后加
  `config.tools.browser.headless = config_extra.browser_silent;` 与
  `config.tools.browser.source = config_extra.browser_source.clone();`
- 更新所有 `NomiResolvedConfig { .. }` 构造点（factory、manager 测试、`provider_health.rs` ×2、`agent_types_integration.rs`）补两字段。

### 4.4 前端改动

- `configKeys.ts`：加 `'agent.browserUse.silent': boolean | undefined;` 与
  `'agent.browserUse.source': 'managed' | 'system' | undefined;`（含注释）。
- `BrowserUseSettingsContent.tsx`：
  - `silent` 用 Arco `Switch`（默认 ON；`configService.get(...) ?? true`），置于 `browserUse` 之下。
  - `source` 用 Arco `Radio.Group`（`managed`/`system` 两项；`?? 'managed'`），受 `browserUse` gating（`disabled={!browserUse}`）。
  - 复用现有 `persistBoolean` 范式;source 用一个字符串版持久化（set + 失败回滚）。
- i18n：`en-US/settings.json` 与 `zh-CN/settings.json` 补
  `browserSilent(/Desc)`、`browserSource(/Desc)`、`browserSourceManaged(/Desc)`、`browserSourceSystem(/Desc)`。

## 5. 平台与范围
- 目标：桌面端（macOS / Windows / 有显示器的 Linux）。
- **无显示器/服务器**：`display_available()==false` 本就强制 headless，不受 `silent` 影响（维持现状）。
- **`system` 来源仅桌面端有意义**（需本机装了 Chrome/Edge）；未探到系统浏览器时**优雅回退** CfT（不报错）。
- 只影响 agent 的 Browser 工具链路;知识抓取与 MCP stdio 桥不变。

## 6. Phase 2（后续）：`system` 来源的「登录」按钮
在 `BrowserUseSettingsContent` 增「登录我的浏览器」按钮：拉起一次**可见** headful 引擎会话指向共享 profile，用户登录常用站点，关闭后 `storage_state` 存入保险库（持久登录）。需要新增后端命令 + IPC。**独立于 §4 的两个开关**，可后置。

## 7. Phase 3（后续）：静默下的 takeover 截图审批
`takeover` 默认 OFF。当用户开启 takeover 且处于静默时，`bring_to_front`（headful-only）无意义。改为：需人工确认时经审批 gate 向 UI 浮**当前页截图 + 批准/拒绝**（引擎已有截图能力）。触及 approval gate / 截图管线，单独一刀。

Phase 1（§4 两个开关）不改 takeover：仅记录「开启 takeover 时建议切可见」，且现有审批 gate 的批准/拒绝仍工作（只有『把窗口弹到前台观察』在 headless 下 no-op）。

## 8. 未来：app 内置浏览器（诉求 3，低优）
构件已具备：`ui/.../WebviewHost.tsx`（沙箱 iframe，Tauri/WebUI 通用）可做**只读镜像**（渲染引擎截图）；真正的「内置可驱动浏览器」仍需经 CDP 引擎（跨域 iframe 无法驱动）。作为独立项目，不在本设计范围。

## 9. 测试策略
- **引擎单测**：`resolve_local_chrome` —— System 优先系统浏览器 / Managed 优先 CfT / 双缺回退 / env 覆写；`ChromeSource::from_source_str`；`EngineConfig::default().chrome_source==Managed`。
- **配置单测**：`BrowserConfig` 默认 `source=="managed"`;layer-merge 覆盖。
- **工厂单测**（如现有 pref 测试范式）：`read_string_pref` 缺省/命中;silent 缺省=true。
- **前端**：typecheck 通过;设置项渲染 + 持久化正确 key。
- **集成/手验**（`--ignored` 或手动）：默认无弹窗;关静默弹窗;`system` 用系统 Chrome 路径（复用 `detect_system_browser_in` 的 `--ignored` 本机测试）。

## 10. 回归与迁移
- **行为变化**：桌面端默认从「弹窗可见」变为「静默」（用户明确要求）。老用户无该 pref → 读缺省即静默。
- config.toml `[tools.browser] headless` 仍受支持;桌面会话 pref 优先。
- 全部改动 additive + 向后兼容;三处引擎构造点其余两处不变。
