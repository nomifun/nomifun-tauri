# Computer Use 与 Browser Use（计算机控制与浏览器自动化）

NomiFun agent 内置/接入两项可选的系统级能力：

- **Computer**（computer use，进程内 Rust）：截屏、鼠标键盘合成输入、窗口枚举/聚焦——让 agent 看到并操作本机桌面。crate：`nomi-computer`（xcap + enigo）。
- **Browser**（browser use，进程内自研 CDP 引擎）：通过内置浏览器引擎驱动 Chrome 完成导航、读取、点击、填表等，以单工具 `Browser` 暴露。crate：`nomi-browser-engine`（自研 Rust CDP）+ `nomi-browser`（facade）。首次启用时引擎按需自动获取 Chrome（`acquire.rs` 内置 CfT 下载/解压），无需 Node/npm。由 `nomi-agent::bootstrap` 在启用且 `browser-use` feature 开启时注册 `BrowserTool`。

> 注：早期的外接 `@playwright/mcp` sidecar 与其 boot-time provisioning（装 node/npm/Chromium）**已移除**；browser use 现统一走进程内自研 CDP 引擎，是唯一浏览器路径。ACP/codex 经 `mcp-browser-stdio`（native facade）接入同一引擎。
>
> 当前文档只描述已落地路径：桌面端的系统设置开关、进程内
> browser/computer 工具，以及对应的 build feature 门控。

两者都是高权限能力。当前桌面产品构建在对应 feature 存在时默认开启，
用户可在系统设置中关闭；无头 Web/服务器构建则不承诺桌面控制或托管
浏览器能力。

## 启用与关闭方式

### 1. 桌面端系统设置（推荐）

桌面应用在系统设置中提供两个页面：

- **Browser Use**（`/settings/browser-use`）
- **Computer Use**（`/settings/computer-use`）

当前桌面构建默认把两个开关设为开启；关闭任一开关会持久化到用户偏好，
后续新会话不会获得对应能力。

### 2. 会话级

创建会话时在 `extra` 中传开关（camelCase 与 snake_case 均可）：

```json
{ "computerUse": true, "browserUse": true }
```

### 3. 宿主级环境变量

```bash
NOMIFUN_COMPUTER_USE=1   # 所有 nomi 会话默认启用 Computer
NOMIFUN_BROWSER_USE=1    # 所有 nomi 会话默认启用 Browser（进程内 native CDP 引擎）
```

### 4. nomi CLI / 配置文件

`~/.nomi/config.toml` 或项目 `.nomi/config.toml`：

```toml
[tools]
max_recent_images = 3        # 历史中保留的工具结果图片总数（旧图自动剥离省 token）

[tools.computer]
enabled = true
max_screenshot_edge = 1568   # 截图长边像素上限

[tools.browser]
enabled = true
headless = false             # 服务器部署建议 true
allowed_origins = []         # 可选 origin 白名单；空=全放行，仅纵深防御
# 注：browser_path / idle_timeout_secs 已弃用（native 引擎自管浏览器与生命周期），保留 #[serde(default)] 仅为旧配置兼容。
```

启用 Browser 后，native 引擎首次使用时自动获取 Chrome（CfT 下载到引擎专属 user-data-dir，不污染用户浏览器），无需预装 Node/npm/Playwright。

## 构建形态（feature 门控）

| 宿主 | Computer（进程内） | Browser（进程内 native CDP） |
|---|---|---|
| 桌面应用（nomifun-desktop） | ✅ 默认编译（`computer-use` feature） | ✅（`browser-use` feature；首次自动获取 Chrome） |
| nomi CLI | ✅ 当前 `nomi-cli` manifest 启用 | ❌ 当前 `nomi-cli` manifest 未启用 |
| Web/服务器（nomifun-web、Docker） | ❌ 不编译（无显示器；xcap/enigo 不进二进制） | ❌ 当前 headless web host 未启用 `browser-use` feature |

`computer-use` feature 链：`apps/desktop` → `nomifun-app` → `nomifun-ai-agent` → `nomi-agent` → `nomi-computer`。Web 构建若配置中误开 computer，仅记录 warning，不报错。Browser 由 `browser-use` feature 门控（`nomi-browser` / `nomi-browser-engine`）。

## macOS 权限

Computer 能力首次使用需在「系统设置 → 隐私与安全性」中授权宿主应用：

- **辅助功能（Accessibility）**：鼠标键盘合成输入需要此项（未来 a11y 树读取/动作亦只需此项）。
- **屏幕录制（Screen Recording）**：截图需要此项（截图全黑或失败时检查）。

当前为反应式诊断：权限缺失时，工具结果会给出授权指引。

## 工具语义与审批

- Computer 为单工具 + `action` 参数形态。
- 只读 action（`screenshot`、`cursor_position`、`list_windows`、`wait`）按 **Info** 类审批——AutoEdit/Default 模式自动放行；操作类 action（点击、输入、滚动、拖拽、`focus_window` 等）按 **Exec** 类——Default 模式需用户确认。
- Plan mode 下 Computer 整工具不可见（只读规划阶段不操作桌面）。
- Browser（native CDP）工具按动作语义派生审批类别：只读观察（如 `observe`/快照）→ Info，写操作（导航、点击、输入等）→ Exec。
- 推荐工作流：`screenshot` 观察 → 操作 → 再次 `screenshot` 验证。

## 截图与 token 治理

- 截图自动降采样到长边 ≤ `max_screenshot_edge`（默认 1568px，Anthropic 视觉推荐区间），文本中标注缩放后尺寸；模型给的坐标自动映射回真实屏幕（含 Retina 缩放）。
- 历史消息中只保留最近 `max_recent_images`（默认 3）张工具结果图片，并受每次请求最多 20 张的提供商兼容上限约束；超出的附件会在轮次结束时剥离，但保留文本和省略说明，避免会话文件与请求 token 膨胀。
- OpenAI 协议的 tool 消息不支持图片：图片以紧随其后的 user 消息（`image_url` data URI）传递，并标注来源 call id。Anthropic/Bedrock/Vertex 走原生 `tool_result` 图片块。
- 外接 MCP 工具回传的图片同样经 `McpToolProxy` 映射进图片管道（单图 ≤ 5 MiB 上限）。

## 替代路径：其他外接 MCP

除内置 Computer 与 native Browser 外，仍可外接任意社区 MCP server（在 MCP 设置中添加），与上述能力互不冲突（工具名不同）。
