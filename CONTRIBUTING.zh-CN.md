# NomiFun 贡献手册

English: [CONTRIBUTING.md](CONTRIBUTING.md)

NomiFun 是一个 Rust + Tauri + React monorepo，也是一套本地优先的高权限自动化系统：它可以驱动 shell、文件、浏览器、桌面应用、智能体、MCP server 和远程能力 API。我们非常欢迎贡献，但贡献必须让维护者能快速理解、快速验证，并且不伤害用户数据与本地安全承诺。

这份手册是本仓库的开发与提交约定：如何选题、如何改代码、如何验证、如何提交一个维护者愿意认真 review 的 PR。

## 先记住这几条

如果你只想快速掌握方向，先记住下面 6 条：

1. PR 要小。一个问题、一个行为变化、一个能说清楚的审查故事。
2. 大功能、新顶层页面、数据库迁移、安全/权限相关逻辑、内置资产、vendored code、大范围重构，先开 issue 或讨论，不要直接开大 PR。
3. 尊重仓库边界：前端在 `ui/`，后端功能在 `crates/backend/`，agent 引擎在 `crates/agent/`，后端需要 agent 能力时优先走 `nomifun-ai-agent` 这条边界。
4. 跑能证明你改动的最小检查。跑不了就如实写在 PR 里，不要假装跑过。
5. 不提交密钥、本地数据、构建产物、私人工作区、专有素材、许可证不明确的第三方资产。
6. 用户能感知的变化，要同步补齐用户侧证据：UI 截图、文档、双语 i18n、需要时更新 changelog。

## 可以贡献什么

最适合起步的贡献通常很小、很具体：

| 方向 | 适合的例子 |
| --- | --- |
| 文档 | 澄清安装步骤、修复过期路由名、补排障说明、同步中英文兄弟文档。 |
| 前端细节 | 修一个明确 UI bug、改善已有控件、补 loading/empty/error 状态。 |
| i18n | 补 `zh-CN` / `en-US` 文案、重命名含义不清的 key、清掉硬编码用户可见文本。 |
| 后端正确性 | 增加校验、修接口边界情况、收紧已有 service 的职责边界。 |
| 测试 | 围绕 bug、迁移、parser、router helper、repository method 增加聚焦测试。 |
| 打包/发布 | 改进构建步骤、release script、updater 说明、平台检查。 |

下面这些工作请先开 issue 或讨论，确认方向后再写代码：

- 新页面、新路由、新能力域、新自动化权限；
- 新数据库表、鉴权/session 行为、token 处理、公开 API；
- 新模型/供应商行为、browser/computer use 权限、agent 编排语义；
- 大型 UI 重设计或跨页面风格调整；
- 新依赖、内置 skill、vendored code、二进制资产、第三方图片/媒体；
- 发布、签名、updater、安装器、数据迁移；
- 触碰大量无关文件的清理型 PR。

如果你想先拿方向，欢迎开 draft PR。请把 PR 标成 draft，附一个短任务清单，并说明你现在需要哪类反馈。

## 社区要求

请遵守 [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)。我们欢迎直接、事实清楚的技术讨论；不接受人身攻击、骚扰、歧视或无关争吵。

安全问题请按 [SECURITY.md](SECURITY.md) 私下报告，不要直接开公开 issue。NomiFun 能操作本地工具和高权限自动化能力，安全报告必须谨慎处理。

可以使用 AI 辅助贡献，但你仍然要对每一行负责：自己审查生成代码，删除不存在的 API，不把私有/受限代码贴进 prompt。若 AI 大量参与了实现，建议在 PR 备注中说明，方便 reviewer 判断风险。

## 本地环境

依赖要求：

| 工具 | 要求 |
| --- | --- |
| Rust | stable toolchain，workspace 使用 edition 2024。 |
| Bun | `>= 1.3.13`，用于前端脚本和辅助工具。 |
| Tauri CLI v2 | 通过仓库依赖安装，由 Bun script 调用。 |
| Git | 支持常规 fork/branch workflow 的近期版本。 |
| Native build tools | 各平台 WebView、TLS、SQLite、libgit2 与编译工具链。 |

安装与基础检查：

```bash
git clone <repo-url> nomifun-tauri
cd nomifun-tauri
bun install
cargo check --workspace
```

常用开发命令：

| 命令 | 什么时候用 |
| --- | --- |
| `bun run dev:ui` | 只改前端，使用 Vite 热更新；没有后端时部分 API 会失败。 |
| `bun run dev:web` | 浏览器 + 后端联调，本地开发默认关闭鉴权。 |
| `bun run serve:web` | 从源码启动更接近生产形态的 web host；需要先有 `ui/dist`。 |
| `bun run dev` | 桌面/Tauri 开发，使用内嵌后端。 |
| `bun run build:ui` | 构建 React SPA。 |
| `bun run build` | 构建当前 OS 的桌面包。 |

优先使用根目录脚本，不要随手写一套裸命令；这些脚本带有本仓库需要的清理与一致性检查。脚本真相来源是 [package.json](package.json)。

## 仓库地图

| 路径 | 负责什么 |
| --- | --- |
| `apps/web` | 独立 `nomifun-web` server，提供 API + SPA。 |
| `apps/desktop` | Tauri 桌面壳，内嵌后端和桌面插件。 |
| `crates/agent` | `nomi-*` crates，独立 agent 引擎。 |
| `crates/backend` | `nomifun-*` crates，HTTP/WS 后端、数据层、鉴权、功能域、公开 API。 |
| `crates/shared` | 少量跨 backend/agent 的共享工具。 |
| `ui` | React 19 + TypeScript + Vite 单页应用。 |
| `docs` | 当前用户文档、运维文档、架构文档、贡献者文档。 |
| `packaging` | 平台打包与部署辅助文件。 |

移动代码前请先读 [docs/contributing/project-structure.md](docs/contributing/project-structure.md)。前端路由真相来源是 `ui/src/renderer/components/layout/Router.tsx`；不要把已经重定向的旧路由写成当前主导航。

## 工程规范

### 通用原则

- 优先复用现有模式，不急着发明新抽象。
- 改动范围收在真正拥有该行为的模块里。
- 能用类型表达的约束，就不要只靠注释或调用者记忆。
- 注释只解释不明显的意图、不变量或集成约束。
- 行为变化要同步文档；过期文档也是 bug。
- 不引入违反本地优先承诺的 telemetry、云依赖或后台数据外发。

### Rust 与后端

- Rust 改动提交前运行 `cargo fmt`。
- 共享 Rust 依赖统一走根目录 [Cargo.toml](Cargo.toml) 的 workspace dependency。
- 后端功能代码放进拥有它的 `nomifun-*` crate。`nomifun-app` 主要负责组合、启动、router glue，不要堆业务逻辑。
- 后端 crate 需要 agent 类型时，通常通过 `nomifun-ai-agent::{nomi_config, nomi_types, RequirementSink}`。新增 backend -> `nomi-*` 直连依赖必须写清理由，通常还要 feature-gated。
- HTTP request/response DTO 如果属于 API contract，放在 `nomifun-api-types`。
- 数据库 schema 变化使用 `crates/backend/nomifun-db/migrations/` 下追加编号 SQL 文件，同时更新 model、repository 和聚焦测试。
- 面向用户的后端失败用 `AppError`，日志用 `tracing` 保留有用上下文。
- async request path 上不要无隔离地做阻塞工作。
- parser、migration、repository、安全校验、bugfix 都应该有聚焦测试。

### 前端

- Renderer 是 React 19 + TypeScript，`strict` 已开启。API 和组件边界的类型要清楚。
- 使用已有 alias：`@/`、`@common/`、`@renderer/`，不要写脆弱的深层相对路径。
- 产品操作主要走 `ui/src/common/adapter/` 下的 HTTP/WebSocket bridge。Tauri 特有逻辑必须封在 platform/adapter 层，不要散落在页面组件里。
- 用户可见文案必须走 i18n。同步更新 `ui/src/renderer/services/i18n/locales/zh-CN/` 和 `ui/src/renderer/services/i18n/locales/en-US/`，然后运行 `bun run gen:i18n` 或 `bun run check:i18n`。
- 主题相关改动使用 `ui/src/renderer/styles/themes/` 下的语义 token，并通过 `bun run check:theme`。
- 优先沿用已有 Arco、UnoCSS 和本地组件模式。修一个小控件，不要顺手重做整个页面。
- 可见 UI 改动尽量附截图或短录屏。

### 文档

- 当前文档放在 `docs/getting-started`、`docs/guides`、`docs/architecture`、`docs/reference`、`docs/contributing`。
- 有中英文兄弟文档时保持同步。
- 容易漂移的实现事实优先链接源码，不要复制一份长篇状态。
- 增删替换截图时同步更新 `docs/images/SCREENSHOTS.md`。
- 不要把历史设计稿、审计稿重新包装成当前事实。

### 依赖、资产与许可证

NomiFun 使用 Apache-2.0。你的贡献会按同一许可证接收。

新增依赖、资产、内置 skill、模型 preset 或生成文件前，请确认：

- 许可证允许在本仓库中再分发；
- 资产不是专有产品或私人项目中复制来的；
- 依赖确实是运行时或构建期需要，而不是“顺手方便”；
- 没有削弱本地优先、无遥测承诺；
- 没有带入密钥、token、私人会话、本地数据。

许可证或来源说不清，就不要加。先开 issue。

## Commit 规范

尽量使用 Conventional Commit 风格：

```text
<type>[optional scope]: <short imperative summary>
```

常用类型：

| 类型 | 适用场景 |
| --- | --- |
| `feat` | 用户可见的新功能或能力。 |
| `fix` | bug 修复。 |
| `docs` | 纯文档。 |
| `refactor` | 不改变行为的代码结构调整。 |
| `perf` | 性能改进。 |
| `test` | 测试或测试辅助代码。 |
| `build` | 构建、打包、依赖、发布工具。 |
| `chore` | 无用户可见行为的维护工作。 |
| `style` | 纯格式化。 |

示例：

```text
fix(conversation): preserve selected model after retry
docs: expand contributor verification ladder
build(mac): generate updater latest.json after signed bundle
```

标题说不清动机、取舍或迁移风险时，写 commit body。公开历史优先英文；中文也可以，但标题必须具体，不能只写“修复问题”“更新文档”。

## 验证阶梯

跑能覆盖你改动的最小检查，并把命令写进 PR。

| 改动类型 | 最小有效检查 |
| --- | --- |
| 纯 Markdown/docs | `git diff --check`；能点链接就点一下，至少人工检查改过的链接。 |
| 根脚本/help | `bun run help --check`。 |
| 前端 TypeScript | `bun run typecheck`。 |
| i18n | `bun run check:i18n`。 |
| 主题/token | `bun run check:theme`。 |
| 前端功能 | `bun run check`；可见 UI 附截图。 |
| Rust 编译路径 | `cargo check --workspace` 或更窄的 `cargo check -p <crate>`。 |
| Rust 行为 | `cargo test -p <crate>` 或覆盖改动的聚焦测试。 |
| 数据库迁移 | migration test 或 repository 聚焦测试；可行时跑 `cargo test -p nomifun-db`。 |
| 打包/发布 | 相关 build script 加你修改过的 release docs。 |
| 安全敏感路径 | 聚焦测试、PR 中写 threat-model 备注；私密漏洞不要公开细节。 |

大范围提交前建议跑：

```bash
cargo check --workspace
bun run check
```

Rust 改动较多时补充聚焦测试：

```bash
cargo test -p <crate>
```

如果命令太慢、平台不支持、或你机器上跑不了，不要假装通过。在 PR 写 `Not run` 并说明原因。

## PR 提交清单

标成 ready for review 前，请确认：

- PR 只有一个清晰目的。
- 标题说明改了什么，而不是只写 issue 编号。
- 描述里讲清用户影响、实现形态和风险。
- 完整解决 issue 才用 `Fixes #123`；部分推进用 `Refs #123` 或 `Towards #123`。
- 列出实际运行过的测试/检查命令。
- UI 改动附截图，或者说明为什么不需要截图。
- 行为需要时，同步更新文档、i18n、changelog、截图 manifest、release notes。
- 没有提交密钥、本地路径、生成构建产物、许可证不兼容资产。

如果改动触及架构、安全、数据迁移或长期维护成本，维护者可能会要求拆小 PR、补测试、补文档或先做设计讨论。

## Changelog 与 release notes

`CHANGELOG.md` 是给人读的，不是 commit log 转储。下面这些变化应该在 `Unreleased` 中补一条：

- 新功能；
- 行为变化；
- 用户可感知 bugfix；
- 安全相关变化；
- 打包/updater 变化；
- 破坏性配置、数据、API 或流程变化；
- PR 带来的已知限制或移除的已知限制。

维护者可能会在发版前重写 changelog 文案。

## Review 协作方式

- 收到 review 后，用代码修改、解释或明确的不同意理由回应。
- 可以 push back，但要基于行为、测试、代码边界和用户影响讨论。
- 处理完 review 且线程安静一段时间后，可以留一句 `PTAL`。
- 有冲突时及时从最新 main rebase 或 merge。
- 除非维护者要求，不要在活跃 review 中 force-push 无关历史重写。

## 参考的开源社区实践

这份指南是为 NomiFun 定制的，但借鉴了这些成熟实践：

- [GitHub contributor guideline docs](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors)
- [Open Source Guides: How to Contribute](https://opensource.guide/how-to-contribute/)
- [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)
- [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/about.html)
- [Kubernetes pull request process](https://www.kubernetes.dev/docs/guide/pull-requests/)
- [scikit-learn contributing guide](https://scikit-learn.org/stable/developers/contributing.html)
