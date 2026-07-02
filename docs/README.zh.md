# NomiFun 文档

本目录保存 **NomiFun** 当前的技术文档、运维文档与贡献者文档。这里只保留与
当前实现一致的内容；历史设计稿与审计记录不在仓库中维护，需要时请查阅 git
历史。

> 初次接触项目请从
> [入门 -> 项目介绍](getting-started/introduction.zh.md) 开始。
> English docs start at [README.md](README.md).

## 从这里开始

| 目标 | 阅读 |
| --- | --- |
| 了解 NomiFun 是什么 | [getting-started/introduction.zh.md](getting-started/introduction.zh.md) |
| 安装或本地运行 | [getting-started/installation.zh.md](getting-started/installation.zh.md) |
| 快速试用 | [getting-started/quick-start.zh.md](getting-started/quick-start.zh.md) |
| 理解当前架构 | [architecture/overview.zh.md](architecture/overview.zh.md) |
| 构建或打包项目 | [contributing/building-and-packaging.zh.md](contributing/building-and-packaging.zh.md) |
| 查询参数、环境变量或 API 分组 | [reference/](reference/) |
| 参与贡献 | [../CONTRIBUTING.zh-CN.md](../CONTRIBUTING.zh-CN.md)、[../CONTRIBUTING.md](../CONTRIBUTING.md) |
| 社区行为准则 | [../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) |
| 报告安全问题 | [../SECURITY.md](../SECURITY.md) |
| 版本记录与发布流程 | [../CHANGELOG.md](../CHANGELOG.md)、[../RELEASING.md](../RELEASING.md) |

## 当前文档

```text
docs/
├── getting-started/      介绍、安装、快速开始
├── guides/               当前产品与运维指南
├── architecture/         当前系统架构与实现地图
├── reference/            配置、API 概览、排障、FAQ
├── contributing/         开发、项目结构、构建与打包
├── skills/               面向外部 agent 的 skill 文档
└── images/               截图清单与图片资源
```

当前顶层用户界面包括会话、终端、模型管理、助手、MCP、开放能力、
需求/AutoWork、定时任务、伙伴、知识库，以及 feature-gated 的
computer/browser 自动化能力。前端路由真相来源是
`ui/src/renderer/components/layout/Router.tsx`。

## 编辑规则

- 有中英文兄弟文件时，保持两者同步。
- 实现事实优先链接源码，不重复容易漂移的细节。
- 不把已重定向的旧 UI 路径写成主导航。
- 如果某功能没有出现在 `Router.tsx` 的当前产品路由中，即使后端仍有
  route，也不要把它写成活跃用户功能。
- 脚本说明以 `package.json` 和 `bun run help` 为准。
