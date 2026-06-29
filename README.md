<a name="top"></a>

<div align="center">

<a href="https://www.nomifun.com">
  <img src="docs/images/brand/og-cover.svg" alt="NomiFun — Fully open-source, local-first super AI workstation" width="820">
</a>

<h3>A no-holds-barred, fully open-source, <em>local-first</em> super AI workstation.</h3>

<p>
  Rich, inventive capabilities and serious productivity gains —<br/>
  with <b>all your data staying on your own machine</b>. Safe for individuals and enterprises, free to commercialize, open to audit.
</p>

<p>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/License-Apache_2.0-FF6F91?style=for-the-badge"></a>
  <img alt="Platform" src="https://img.shields.io/badge/Platform-macOS%20%7C%20Windows%20%7C%20Linux-7583B2?style=for-the-badge">
  <img alt="Status" src="https://img.shields.io/badge/Status-pre--1.0-FBBF24?style=for-the-badge">
  <a href="https://www.nomifun.com"><img alt="Website" src="https://img.shields.io/badge/Website-nomifun.com-FF6F91?style=for-the-badge"></a>
</p>

<p>
  <img alt="Built with Tauri 2" src="https://img.shields.io/badge/Tauri-2-24C8DB?style=flat-square&logo=tauri&logoColor=white">
  <img alt="Rust 2024" src="https://img.shields.io/badge/Rust-edition_2024-CE412B?style=flat-square&logo=rust&logoColor=white">
  <img alt="React 19" src="https://img.shields.io/badge/React-19-61DAFB?style=flat-square&logo=react&logoColor=white">
  <a href="https://github.com/nomifun/nomifun-tauri/stargazers"><img alt="Stars" src="https://img.shields.io/github/stars/nomifun/nomifun-tauri?style=flat-square&color=FF6F91"></a>
</p>

<p>
  <b>English</b>&nbsp;·&nbsp;<a href="README.zh-CN.md">简体中文</a>
</p>

<p>
  <a href="https://www.nomifun.com">🌐 Website</a>&nbsp;·&nbsp;
  <a href="docs/README.md">📖 Docs</a>&nbsp;·&nbsp;
  <a href="#-getting-started">🚀 Get started</a>&nbsp;·&nbsp;
  <a href="https://github.com/nomifun/nomifun-tauri/releases">📦 Releases</a>&nbsp;·&nbsp;
  <a href="#-contact--community">💬 Community</a>
</p>

</div>

---

**NomiFun** is everything you imagine an AI workstation to be — and it runs on your terms. One React frontend and one Rust backend give you an evolving desktop companion, an unattended automation platform, a unified knowledge base, native computer- and browser-use, and an open capability bus that any agent can drive. No cloud account. No telemetry. No subscription. Your data never leaves your machine except for the LLM calls **you** configure.

> The product name is **NomiFun**. Lowercase `nomifun` is used only for code identifiers, crate names, environment variables, and repository paths.

---

## ✨ Why NomiFun

|  | |
|---|---|
| 🔓 **Open & local** | Source fully open, no reservations. Data lives on your machine and is never sent out on its own. Free for personal **and** commercial use. Open to audit. |
| 🐾 **Evolving companions** | The most complete companion-growth system we know of — it learns how you work and gets better over time. Not just a buddy, a genuine productivity partner. |
| 🤖 **Unattended automation** | Manage requirements, then just give the order. AutoWork + IDMM keep your sessions alive and working reliably while you're away. |
| 🌐 **Open capability ecosystem** | Everything is here, everything connects, everything cooperates — and *any* agent can borrow NomiFun's powers over MCP / REST. |
| 🧩 **Config once, use anywhere** | Unified management of knowledge bases, skills, agents, MCP servers, and models — defined once, reused across every surface. |
| 🖥️ **Truly native** | In-process, self-built **computer use** and **browser use** as native tools — more capable, faster, and cheaper on tokens. |
| 🚀 **Built for productivity** | Designed from real needs, with a lot of inventive capabilities. And many delightful features are still on the way. |

---

## 🔒 Local-first, by design

Data security is not a setting in NomiFun — it is the architecture.

- **All data is local.** NomiFun never proactively sends your data anywhere. The **only** outbound network calls are the LLM requests you explicitly configure to your chosen model provider. There is no other third-party service integration phoning home.
- **Safe for anyone who cares about data.** Individuals and enterprises with strict data-handling requirements can use it with confidence. The code is **fully open and open to audit**.
- **We cut features to keep this promise.** To guarantee your data stays yours, we deliberately dropped several advanced, genuinely fun feature designs. Everything here is in service of letting users — and developers — relax.
- **No ads. No commercialization. No membership tiers.** We promise to *never* charge for any feature of this project. The only thing that costs money is your LLM provider's tokens, which is outside our control. (If finding/serving models is painful, [reach out](#-contact--community) — we're happy to help build a unified model gateway.)

See [`SECURITY.md`](SECURITY.md) for the deployment threat model and responsible-disclosure policy.

---

## 🖼️ A look inside

<div align="center">

<p>
  🎬 <b>Intro video:</b> <a href="https://www.youtube.com/watch?v=Z28XyhvNh_E">https://www.youtube.com/watch?v=Z28XyhvNh_E</a>
</p>

<p>
  <img src="docs/images/readme-01-workbench-overview.png" alt="NomiFun desktop workbench with conversation, companion, and project metrics" width="100%">
  <br/><sub><b>Desktop workbench: conversation, companion, and live session metrics</b></sub>
</p>

<table>
  <tr>
    <td width="50%"><img src="docs/images/gs-01-introduction-hero.png" alt="Home / new session"><br/><sub><b>Home & sessions</b></sub></td>
    <td width="50%"><img src="docs/images/channels-01-overview.png" alt="Companion remote channels"><br/><sub><b>Companion · IM channels</b></sub></td>
  </tr>
  <tr>
    <td width="50%"><img src="docs/images/autowork-03-kanban.png" alt="Requirements board"><br/><sub><b>Requirements · AutoWork board</b></sub></td>
    <td width="50%"><img src="docs/images/webui-01-settings-overview.png" alt="Open capabilities"><br/><sub><b>Open capability bus</b></sub></td>
  </tr>
  <tr>
    <td width="50%"><img src="docs/images/terminal-03-driving-session.png" alt="Terminal session"><br/><sub><b>Agent-driven terminal</b></sub></td>
    <td width="50%"><img src="docs/images/webui-04-qr-login-phone.png" alt="Phone QR login"><br/><sub><b>WebUI · scan-to-connect</b></sub></td>
  </tr>
</table>

<sub>Real in-app captures. See <a href="docs/images/SCREENSHOTS.md">the screenshot manifest</a> for the full set and capture method.</sub>

</div>

---

## 🚀 Feature highlights

### 🐾 Desktop Companion — it grows with you

> Guide: [`docs/guides/companions.md`](docs/guides/companions.md)

The companion you talk to every day quietly becomes the assistant who *gets* you.

- **Make it yours.** Upload a custom companion figure (DIY), or pick from an independent figure library decoupled from any single companion.
- **One brain, many faces.** Run multiple companions that share a common memory hub, while each keeps its own **private** memory and can mount different domain knowledge bases. Teach *one* companion well, then have it teach the others.
- **It learns you (opt-in, on by default after a one-time consent).** A background learner distills your usage into durable memories; a deterministic evolution engine mines your recurring multi-step tool sequences into **draft skills** it proposes for your review. Memory is fully **visible and editable**.
- **Skills that spread.** Companions generate their own skills, discuss them with you, and can **gift** a skill to another companion (the recipient gets a copy) — opt-in shared learning across your roster.
- **A super gateway, not just a buddy.** Each companion is a complete, independent individual that can connect to multiple IM channels. From anywhere with a network and a chat app, message your companion to drive your computer for you. Each companion can fully operate the desktop's capabilities.

### 🤖 Unattended automation — Requirements + AutoWork + IDMM

> Guides: [`autowork-requirements.md`](docs/guides/autowork-requirements.md) · [`intelligent-decision.md`](docs/guides/intelligent-decision.md)

You give the orders; NomiFun reliably does the work.

- **Requirement platform** — a CRUD store with ordered rotation, a board/kanban, tags, and per-item claim.
- **AutoWork** — claims pending requirements, drives a turn, rotates to the next, and renews leases while a turn is in flight. Targets can be **conversation agents *or* terminal PTYs**.
- **IDMM (Intelligent Decision-Making)** — per-session supervision that keeps agents alive through provider faults and decision stalls, with a no-LLM rule tier and a sidecar backup-model tier, stacking on top of AutoWork.
- **Notify out** — completion notifications to **Lark/Feishu** custom bots, **Slack**, and HTTP webhooks.

### 📚 Unified Knowledge Base

> Guide: [`docs/guides/mcp-and-skills.md`](docs/guides/mcp-and-skills.md)

Pull the knowledge scattered across your system into one managed, trackable place.

- **Centralized management & tracking** — create, mount, and track consumers across conversations, terminals, and companions.
- **Safe write-back** — a code-enforced, per-surface write policy. By default, writes are **staged into a review inbox** with unified-diff preview and merge/discard — so agents never scribble into the wrong place.
- **Real-time URL snapshot** — turn any web page into a knowledge source (SSRF-guarded fetch, HTML→Markdown), in *snapshot* (persisted, re-fetchable) or *live* mode.
- **Scoped retrieval** — agents call a `knowledge_search` tool whose scope is decided server-side and cannot be widened.

### 🖥️ Native Computer Use & Browser Use *(desktop build)*

> Guide: [`docs/guides/computer-browser-use.md`](docs/guides/computer-browser-use.md)

Self-built, **in-process Rust** — no Playwright, no Node, no third-party automation daemon. More capable, faster, and far cheaper on tokens, with fine-grained control and fully open source for you to extend.

- **Computer use** — accessibility tree + Set-of-Marks overlay + OCR, steering the model to act on real UI elements instead of guessing pixels. macOS (AXUIElement + Vision OCR) and Windows (UI Automation) are complete; Linux (AT-SPI2) is partial.
- **Browser use** — an in-process Chromium CDP engine with ARIA observation, an egress **firewall** with out-of-band approval, and an origin-bound secret vault so credentials never reach the LLM.
- **Guarded by design** — every action carries a danger × surface approval matrix; irreversible actions wait for explicit confirmation.

> ℹ️ Computer/browser control ship with the **desktop app**. The headless web/server host omits them by design.

### 🌐 Open capability bus — MCP + REST

> Guides: [`remote-capability-api.md`](docs/guides/remote-capability-api.md) · [`remote-capability-api-examples.md`](docs/guides/remote-capability-api-examples.md)

Every capability NomiFun has is exposed through a single, typed capability registry — **~20 domains and 150+ tools** — so you can wire NomiFun into anything.

- **MCP front door** at `/mcp` (authenticated, Streamable-HTTP). Point **Claude Code, Cursor, or your own agent** at it and they operate NomiFun exactly as the desktop companion does.
- **REST + OpenAPI** at `/v1/tools`, with streaming and an auto-generated `/v1/openapi.json`.
- Adding a capability to the bus makes it appear on MCP **and** REST automatically — no drift.

### 🧩 Bring your own agents — or use the built-in one

> Guide: [`docs/guides/model-routing.md`](docs/guides/model-routing.md)

- **Built-in `nomi` agent** — no extra install. Works with **26+ model providers/presets** (OpenAI, Anthropic, Gemini + Vertex AI, AWS Bedrock, DeepSeek, OpenRouter, Moonshot/Kimi, Qwen/Dashscope, Zhipu/GLM, MiniMax, SiliconFlow, xAI, Volcengine/Doubao, and more) across **4 wire protocols**, plus the **New API** aggregator gateway.
- **~19 external agents over ACP** — connect Claude Code, Codex, Gemini, Qwen, Kimi, Cursor, Copilot, Goose, OpenCode, Droid, and more, and NomiFun feeds them models *and* its native capabilities (computer/browser/knowledge/gateway) over injected MCP bridges.
- **Everywhere** — the native capabilities are available to the built-in agent, to ACP agents, in the chat UI, **and** in the terminal.

### 💻 Terminal mode

> Guide: [`docs/guides/terminal.md`](docs/guides/terminal.md)

Run agent CLIs inside in-app PTY sessions (or the standalone `nomi` CLI). NomiFun injects native capabilities — knowledge search, requirement completion, and lifecycle hooks — into known CLIs through their *own* native config, so you keep full fidelity and OAuth.

### 📱 WebUI remote control — scan, and you're in

> Guide: [`docs/guides/webui-remote-access.md`](docs/guides/webui-remote-access.md)

No social platform required. One-tap **QR pairing** connects your phone or tablet to your computer over the LAN (one-time token, realtime over WebSocket) so you can drive your workstation remotely from the couch.

### ⚙️ Config once, use anywhere

Central hubs for **Knowledge**, **Assistants & Skills**, **MCP**, **Models**, and **Open Capabilities** — define them once, then select per conversation, terminal, channel, or companion. One source of truth, reused everywhere.

### 💬 11 IM channels

> Guide: [`docs/guides/channels.md`](docs/guides/channels.md)

Bind a companion to any of these and drive it from where you already chat:

`Telegram` · `Lark / 飞书` · `DingTalk / 钉钉` · `WeChat / 微信` · `Discord` · `Slack` · `Matrix` · `Mattermost` · `Twitch` · `Nostr` · `QQ Bot`

---

## 🏗️ Architecture

One React frontend, one Rust backend, **two host modes** — and the same backend runs in-process in both.

| | `nomifun-desktop` | `nomifun-web` |
|---|---|---|
| **Shell** | Tauri 2 desktop app | Standalone axum server |
| **Backend** | Embedded in-process, private loopback port | Same backend, in-process |
| **Auth** | Local-trust token injected into the webview | Login required by default |
| **Serves** | Native desktop UI + tray + companion windows | API + `/ws` + built SPA on one port |
| **Computer / browser use** | ✅ Included | ❌ Headless (omitted) |

There is no Electron shell, no Node web host, and no prebuilt backend handoff.

<details>
<summary><b>Repository map</b></summary>

```text
apps/
  desktop/      Tauri 2 shell and desktop-only commands
  web/          standalone web host for API + SPA
crates/
  agent/        15 nomi-* crates: engine, providers, tools, MCP, skills, memory,
                browser/computer use, and the standalone nomi CLI
  backend/      29 nomifun-* crates: app composition, auth, database, sessions,
                MCP, knowledge, requirements, terminal, companion, gateway, etc.
  shared/       2 cross-layer crates: nomifun-net and nomi-redact
ui/             React 19 + Vite SPA shared by desktop and web
docs/           technical docs, user/operator guides, architecture notes
packaging/      Linux deployment support for the web host
```

Start with [`docs/architecture/overview.md`](docs/architecture/overview.md) for the full system map. The Cargo workspace is defined in [`Cargo.toml`](Cargo.toml).

</details>

---

## 🚀 Getting started

> ℹ️ There are **no prebuilt installers yet** — install from source or run the server with Docker. Watch [Releases](https://github.com/nomifun/nomifun-tauri/releases) for binaries.

**Prerequisites**

- [Rust](https://rustup.rs) — stable toolchain, edition 2024
- [Bun](https://bun.sh) ≥ 1.3.13
- Recommended on PATH for full agent tooling: `node` / `npm` / `npx`, `git`, `ripgrep`

**Desktop app (from source)**

```bash
git clone https://github.com/nomifun/nomifun-tauri.git
cd nomifun-tauri
bun install

bun run dev      # develop with hot reload
bun run build    # build a desktop bundle for your OS
```

**Web server (self-host)**

```bash
bun run build:ui && bun run serve:web
# serves API + SPA on http://127.0.0.1:8787 (login required)
```

**Docker (self-host the server)**

```bash
docker compose up -d --build
# then open http://<server-ip>:8787  —  pair with the bundled Caddyfile for TLS
```

See [`docs/getting-started/installation.md`](docs/getting-started/installation.md) and [`docs/guides/web-server-deployment.md`](docs/guides/web-server-deployment.md) for details.

---

## 🛠️ Development

```bash
bun install        # install dependencies (one-time)
bun run dev        # desktop app development (hot reload)
bun run dev:web    # web host + Vite development
bun run build:ui   # build the SPA
bun run check      # frontend typecheck + i18n + theme + script-registry gate
bun run test       # Rust tests (use test:fast for nextest)
```

Prefer the scripted entry points over plain `cargo`/`vite` — they include build-dir pruning and consistency checks. New to the codebase? Read [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`docs/contributing/development.md`](docs/contributing/development.md).

<details>
<summary><b>Full script catalog</b></summary>

<!-- BEGIN GENERATED SCRIPTS (bun run help --readme) -->

| 脚本 | 说明 |
| --- | --- |
| **开发（热重载）** | |
| `bun run dev` | 启动桌面应用开发（tauri dev，热重载） |
| `bun run dev:web` | 启动 Web 全栈开发（后端 API + 前端 vite） |
| `bun run dev:ui` | 仅启动前端开发服务器（纯 vite，无后端） |
| **构建（出制品）** | |
| `bun run build` | 为当前操作系统打桌面安装包 |
| `bun run build:signed` | 打桌面包并签名+公证（仅 macOS） |
| `bun run build:updater` | 打桌面包并产出自更新 .sig 制品 |
| `bun run build:ui` | 前端生产构建 → ui/dist |
| **运行（组装好的应用）** | |
| `bun run serve:web` | 启动 Web 服务器，托管已构建的前端 |
| **测试** | |
| `bun run test` | 运行全部 Rust 测试（含 doctest） |
| `bun run test:fast` | 用 nextest 快速跑 Rust 测试（日常） |
| **静态检查 / 门禁** | |
| `bun run check` | 聚合静态门禁：typecheck + i18n + 主题契约 + 脚本登记 |
| `bun run typecheck` | 前端 TypeScript 类型检查（tsc --noEmit） |
| `bun run check:i18n` | 校验 i18n 类型与 locale 键是否一致 |
| `bun run check:theme` | 校验预设 CSS 主题契约 |
| **格式化** | |
| `bun run fmt` | 格式化 Rust 代码（cargo fmt） |
| `bun run fmt:check` | 校验 Rust 代码格式（cargo fmt --check） |
| **代码生成** | |
| `bun run gen:i18n` | 由 locale 重新生成 i18n 类型声明 |
| **维护 / 工具** | |
| `bun run clean` | 深度回收构建空间（debug 产物 + flycheck + 旧安装包） |
| `bun run seed:dev` | 用生产数据目录播种 dev 数据目录 |
| `bun run help` | 打印脚本目录（--check 校验登记 / --readme 生成 README 表） |

<!-- END GENERATED SCRIPTS -->

</details>

---

## 📖 Documentation

- [`docs/README.md`](docs/README.md) — documentation index
- [`docs/getting-started/`](docs/getting-started) — installation and first run
- [`docs/guides/`](docs/guides) — user & operator guides (companions, channels, AutoWork, knowledge, computer/browser use, terminal, remote API, …)
- [`docs/architecture/`](docs/architecture) — technical architecture
- [`docs/reference/`](docs/reference) — configuration, API overview, FAQ, troubleshooting

Docs are bilingual: every page has an English `*.md` and a Simplified-Chinese `*.zh.md` sibling.

---

## 🗺️ Coming soon

NomiFun is **pre-1.0** and built part-time, so there's a lot still in flight. On the horizon: prebuilt installers, inbound issue-tracker / requirement sources, more knowledge connectors (Feishu, and beyond), official desktop binaries — plus a few surprises we're genuinely excited about. **Stay tuned.** ✨

---

## 🤝 Contributing & community

NomiFun very much needs your help to grow — code contributions, community building, and evangelism are all hugely welcome. If you have passion for this project, please [reach out](#-contact--community) and build the NomiFun ecosystem with us.

- Read [`CONTRIBUTING.md`](CONTRIBUTING.md) to get set up and learn the check ladder.
- Be excellent to each other — see [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
- Found a vulnerability? Follow [`SECURITY.md`](SECURITY.md).
- Browse [open issues](https://github.com/nomifun/nomifun-tauri/issues) for a place to start.

---

## 💛 A note from the author

> This is a part-time effort with limited bandwidth, and many delightful features are still on the way. If this resonates with you, join in any way you like — a line of code, a suggestion, a reshare all mean a lot.

NomiFun is **completely open source, with nothing held back**. Individuals and enterprises are free to build on it and use it commercially.

- **Forks & commercial use are welcome.** They're also at your own risk — the author and contributors assume no liability for downstream use. Apache-2.0 requires no permission from us.
- **A friendly heads-up is appreciated, not required.** If you fork or commercialize NomiFun, we'd love a note — *not* as a license condition, simply because knowing the project is valued is the kind of recognition that keeps it going.
- **Some features were intentionally left out of the open-source release** to keep the local-data promise airtight — without the people and funding to guarantee everyone's data security, removing them was the responsible choice. As time and resources allow, we hope to bring more of them to you.

Thank you for being here. 🙏

---

## 🔗 Friendly links

Projects and products we appreciate:

| Product | What it does |
|---|---|
| [Saytive](http://saytive.ai/) | **Be Creative, Be Saytive.** A voice input method for creative workers, using strong models and thoughtful product design to sense your work context and deliver fast, accurate, scene-aware transcription. |
| [Fast](https://fast.saien.pro) | **Search, one tap away.** Type, click, and jump straight to search results across RED, Douyin, Meituan, and dozens of mainstream apps. No feed distraction, just search. |
| [AionUi](https://github.com/iOfficeAI/AionUi) | AionUi ships with a complete AI agent engine. Unlike tools that require separate CLI-agent installs, AionUi works the moment you install it. |

---

## 📬 Contact & community

We'd love to hear from you. The fastest way to reach us is GitHub; the social channels below are all official.

| Channel | Where |
|---|---|
| 🌐 **Website** | [www.nomifun.com](https://www.nomifun.com) |
| 🐙 **GitHub** | [nomifun/nomifun-tauri](https://github.com/nomifun/nomifun-tauri) · [Issues](https://github.com/nomifun/nomifun-tauri/issues) · [Releases](https://github.com/nomifun/nomifun-tauri/releases) |
| ✉️ **Email** | `hello@nomifun.com` <sub>(provisional — being finalized)</sub> |
| 📕 **小红书 / RED** | [NomiFun](https://xhslink.com/m/4x6ti8n6cA1) |
| 📺 **Bilibili** | [NomiFun](https://b23.tv/0UhgKDh) |
| 🎵 **抖音 / Douyin** | [NomiFun](https://v.douyin.com/MDT5QVdYaJk/) |
| ▶️ **YouTube** | [@NomiFun-o2y](https://www.youtube.com/@NomiFun-o2y) |
| 𝕏 **X (Twitter)** | [@colir0](https://x.com/colir0) |
| 🎬 **TikTok** | [@colir0luo](https://www.tiktok.com/@colir0luo) |

**Join the chat groups** — scan to join:

<div align="center">
<table>
  <tr>
    <td align="center"><img src="docs/images/contact/wechat-group-qr.png" alt="WeChat group QR" width="220"><br/><sub><b>WeChat group / 微信群</b></sub></td>
    <td align="center"><img src="docs/images/contact/qq-group-qr.png" alt="QQ group QR" width="220"><br/><sub><b>QQ group / QQ 群</b></sub></td>
  </tr>
</table>
</div>

---

## ⚖️ License

[Apache-2.0](LICENSE) © 2025–2026 NomiFun.

See [`NOTICE`](NOTICE) for third-party attributions, including the [AionUi](https://github.com/iOfficeAI/AionUi) project that NomiFun originally forked from before its current Tauri/Rust architecture.

<div align="center">
<br/>
<sub>Built with 💛 for people who want AI on their own terms.</sub>
<br/><br/>
<a href="#top">⬆ Back to top</a>
</div>
