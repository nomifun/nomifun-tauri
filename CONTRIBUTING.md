# Contributing to NomiFun

Simplified Chinese: [CONTRIBUTING.zh-CN.md](CONTRIBUTING.zh-CN.md)

NomiFun is a Rust + Tauri + React monorepo. It is also a local-first automation
surface that can drive shells, files, browsers, desktop apps, agents, MCP
servers, and remote capability APIs. Contributions are welcome, but they need to
be easy to review and safe for users.

This handbook is the contribution contract for the repository: how to choose
work, how to change code, how to verify it, and how to submit a pull request
that maintainers can review without guessing.

## Read This First

If you only remember one page of rules, remember these:

1. Keep pull requests small. One problem, one behavior change, one reviewable
   story.
2. Ask first before large product changes, new top-level surfaces, database
   migrations, security-sensitive flows, bundled assets, vendored code, or broad
   refactors.
3. Follow the existing ownership boundaries: renderer code through `ui/`,
   backend feature code through `crates/backend/`, agent-engine code through
   `crates/agent/`, and backend-to-agent usage through `nomifun-ai-agent` unless
   a documented exception already exists.
4. Run the narrowest checks that prove your change works. If you cannot run a
   check, say so in the PR and explain why.
5. Do not commit secrets, local data, build output, private workspaces,
   proprietary assets, or third-party assets whose redistribution license is
   unclear.
6. User-visible changes need user-facing notes: screenshots when UI changes,
   docs when workflows change, i18n updates for both supported locales, and
   changelog notes when the change affects a release user.

## What To Work On

Good first contributions are usually small and concrete:

| Lane | Good examples |
| --- | --- |
| Documentation | Clarify setup, fix stale route names, improve troubleshooting, sync English/Chinese siblings. |
| Frontend polish | Fix a narrow UI bug, improve an existing control, add missing loading/empty/error states. |
| i18n | Add missing `zh-CN` / `en-US` strings, rename unclear keys, remove hardcoded text. |
| Backend correctness | Add validation, fix an endpoint edge case, tighten an existing service boundary. |
| Tests | Add focused Rust tests around a bug, migration, parser, router helper, or repository method. |
| Packaging | Improve documented build steps, release scripts, updater notes, or platform-specific checks. |

Open an issue or discussion first when the work changes product direction or
review cost materially:

- new app pages, routes, capability domains, or automation permissions;
- new database tables, auth/session behavior, token handling, or public APIs;
- new model/provider behavior, browser/computer-use permissions, or agent
  orchestration semantics;
- large UI redesigns or cross-cutting style changes;
- dependencies, bundled skills, vendored code, binary assets, or third-party
  media;
- release, signing, updater, installer, or data-migration behavior;
- broad cleanup that touches many unrelated files.

Draft pull requests are welcome when you want early direction. Mark the PR as
draft, include a short task list, and say what kind of feedback you need.

## Community Expectations

Follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md). Be direct, factual, and kind.
Technical disagreement is normal; personal attacks are not.

Report vulnerabilities through [SECURITY.md](SECURITY.md), not a public issue.
NomiFun can operate local tools and high-privilege automation surfaces, so
security reports need private handling.

AI-assisted contributions are allowed. You are still responsible for every line:
review generated code, remove hallucinated APIs, do not paste private or
licensed third-party code into prompts, and disclose material AI assistance in
the PR notes when it helps reviewers understand the work.

## Local Setup

Prerequisites:

| Tool | Minimum / expectation |
| --- | --- |
| Rust | Stable toolchain, edition 2024 workspace. |
| Bun | `>= 1.3.13`; used for frontend scripts and helper tooling. |
| Tauri CLI v2 | Installed through repository dependencies and invoked by Bun scripts. |
| Git | Recent version with normal fork/branch workflow support. |
| Native build tools | Platform-specific WebView, TLS, SQLite, libgit2, and compiler dependencies. |

Install and smoke-check:

```bash
git clone <repo-url> nomifun-tauri
cd nomifun-tauri
bun install
cargo check --workspace
```

Daily development loops:

| Command | Use when |
| --- | --- |
| `bun run dev:ui` | Frontend-only iteration with Vite. Some API calls may fail without a backend. |
| `bun run dev:web` | Browser + backend development with auth disabled for local iteration. |
| `bun run serve:web` | Production-style web host from source; expects `ui/dist` after `bun run build:ui`. |
| `bun run dev` | Desktop/Tauri development with the embedded backend. |
| `bun run build:ui` | Build the React SPA. |
| `bun run build` | Build the desktop bundle for the current OS. |

Prefer root scripts over ad hoc commands because they include repository-specific
cleanup and consistency checks. The script registry is in [package.json](package.json).

## Repository Map

| Path | Ownership |
| --- | --- |
| `apps/web` | Standalone `nomifun-web` server that serves API + SPA. |
| `apps/desktop` | Tauri desktop shell with embedded backend and desktop plugins. |
| `crates/agent` | `nomi-*` crates: the independent agent engine. |
| `crates/backend` | `nomifun-*` crates: HTTP/WS backend, data layer, auth, features, public APIs. |
| `crates/shared` | Rare cross-layer utilities used by both backend and agent code. |
| `ui` | React 19 + TypeScript + Vite SPA. |
| `docs` | Current user, operator, architecture, and contributor documentation. |
| `packaging` | Platform packaging and deployment support files. |

Read [docs/contributing/project-structure.md](docs/contributing/project-structure.md)
before moving code across boundaries. The frontend route map is
`ui/src/renderer/components/layout/Router.tsx`; do not document or promote
legacy redirected routes as current navigation.

## Engineering Standards

### General

- Prefer existing patterns over new abstractions.
- Keep changes scoped to the module that owns the behavior.
- Make invalid states hard to represent where practical.
- Add comments only when they explain non-obvious intent, invariants, or
  integration constraints.
- Update docs beside behavior changes; stale docs are a bug.
- Do not introduce telemetry, cloud dependencies, or background data transfer
  that violates the local-first promise.

### Rust And Backend

- Run `cargo fmt` before submitting Rust changes.
- Use workspace dependencies from the root [Cargo.toml](Cargo.toml) when adding
  shared Rust dependencies.
- Route backend feature code through its owning `nomifun-*` crate. Do not put
  feature logic in `nomifun-app` unless it is composition/boot/router glue.
- Backend crates should normally use agent types through
  `nomifun-ai-agent::{nomi_config, nomi_types, RequirementSink}`. New direct
  `nomi-*` dependencies from backend crates need a documented reason and should
  usually be feature-gated.
- HTTP request/response DTOs belong in `nomifun-api-types` when they are part of
  the API contract.
- Database schema changes are append-only SQL files under
  `crates/backend/nomifun-db/migrations/`. Update models, repositories, and
  focused migration tests together.
- Use `AppError` for user-facing backend failures and preserve useful context in
  logs with `tracing`.
- Avoid blocking work on async request paths unless it is already isolated or
  explicitly documented.
- Add focused tests for parsing, migrations, repository behavior, security
  checks, and bug fixes.

### Frontend

- The renderer is React 19 + TypeScript with `strict` enabled. Keep types
  explicit at API and component boundaries.
- Use the configured aliases (`@/`, `@common/`, `@renderer/`) instead of fragile
  deep relative paths.
- Most product operations go through HTTP/WebSocket bridges in
  `ui/src/common/adapter/`. Tauri-specific behavior must stay behind the
  platform/adapter layer, not scattered through pages.
- User-visible text must go through i18n. Update both
  `ui/src/renderer/services/i18n/locales/zh-CN/` and
  `ui/src/renderer/services/i18n/locales/en-US/`, then run
  `bun run gen:i18n` or `bun run check:i18n`.
- Theme work must use semantic tokens in `ui/src/renderer/styles/themes/` and
  pass `bun run check:theme`.
- Prefer existing Arco, UnoCSS, and local component patterns. Do not restyle a
  whole surface to fix a small control.
- For visible UI changes, include screenshots or a short screen recording when
  practical.

### Documentation

- Keep current docs under `docs/getting-started`, `docs/guides`,
  `docs/architecture`, `docs/reference`, and `docs/contributing`.
- Keep English and Simplified Chinese sibling docs in sync when both exist.
- Link to implementation sources for facts that may drift.
- Update `docs/images/SCREENSHOTS.md` when screenshots are added, removed, or
  replaced.
- Do not reintroduce historical design/audit material as current truth.

### Dependencies, Assets, And Licenses

NomiFun is Apache-2.0. Contributions are accepted under the same license.

Before adding a dependency, asset, bundled skill, model preset, or generated
file, verify:

- the license allows redistribution in this repository;
- the asset is not proprietary or copied from a private product;
- the dependency is needed at runtime or build time, not just convenient;
- the change does not weaken the local-first and no-telemetry promise;
- secrets, keys, tokens, private conversations, and local data are not included.

If the license or origin is ambiguous, do not add it. Open an issue first.

## Commit Messages

Use Conventional Commit style where practical:

```text
<type>[optional scope]: <short imperative summary>
```

Common types:

| Type | Use for |
| --- | --- |
| `feat` | User-visible feature or capability. |
| `fix` | Bug fix. |
| `docs` | Documentation-only change. |
| `refactor` | Behavior-preserving code structure change. |
| `perf` | Performance improvement. |
| `test` | Test-only or test-support change. |
| `build` | Build, package, dependency, or release tooling. |
| `chore` | Maintenance with no user-visible behavior. |
| `style` | Formatting-only change. |

Examples:

```text
fix(conversation): preserve selected model after retry
docs: expand contributor verification ladder
build(mac): generate updater latest.json after signed bundle
```

Use a commit body when the title cannot explain the motivation, tradeoff, or
migration risk. Prefer English for public history when possible; Chinese is
accepted when the context is clearly Chinese, but the title still needs to be
specific.

## Verification Ladder

Run the smallest set that covers your change, then record the commands in the
PR.

| Change type | Minimum useful checks |
| --- | --- |
| Markdown/docs only | `git diff --check`; click or inspect changed links when possible. |
| Root script/help changes | `bun run help --check`. |
| Frontend TypeScript | `bun run typecheck`. |
| i18n changes | `bun run check:i18n`. |
| Theme/token changes | `bun run check:theme`. |
| Frontend feature | `bun run check`; add screenshots for visible surfaces. |
| Rust compile path | `cargo check --workspace` or a narrower `cargo check -p <crate>`. |
| Rust behavior | `cargo test -p <crate>` or the focused test target that covers the change. |
| Database migration | Migration test or focused repository test plus `cargo test -p nomifun-db` when practical. |
| Packaging/release | The relevant build script plus the release docs you changed. |
| Security-sensitive path | Focused tests, threat-model notes in the PR, and no public vulnerability details if disclosure is private. |

For a broad pre-PR pass, run:

```bash
cargo check --workspace
bun run check
```

For Rust-heavy changes, add targeted tests:

```bash
cargo test -p <crate>
```

If a command is slow or unavailable on your machine, do not fake it. State
`Not run` with the reason.

## Pull Request Checklist

Before marking a PR ready for review:

- The PR has one clear purpose.
- The title says what changed, not just which issue number it touches.
- The description explains user impact, implementation shape, and risk.
- Linked issues use `Fixes #123` only when the PR fully resolves them; use
  `Refs #123` or `Towards #123` for partial work.
- Relevant tests/checks are listed with exact commands.
- UI changes include screenshots or a reason screenshots are unnecessary.
- Docs, i18n, changelog, screenshot manifest, and release notes are updated when
  the behavior requires it.
- No secrets, local paths, generated build output, or incompatible assets were
  added.

Expect maintainers to ask for smaller PRs, additional tests, documentation
updates, or a design discussion when the change affects architecture, security,
data migration, or long-term maintenance.

## Changelog And Release Notes

`CHANGELOG.md` is for humans, not a dump of commit messages. Add an
`Unreleased` note when your change matters to users, operators, or downstream
builders:

- new features;
- behavior changes;
- bug fixes users may notice;
- security-relevant changes;
- packaging/updater changes;
- breaking config, data, API, or workflow changes;
- known limitations created or removed by the PR.

Maintainers may rewrite changelog entries during release preparation.

## Review Etiquette

- Respond to review comments with the code change, a clarification, or a
  specific reason you disagree.
- It is fine to push back on a suggestion; ground the discussion in behavior,
  tests, code ownership, and user impact.
- After addressing review, leave a short `PTAL` comment if the thread has been
  quiet.
- Rebase or merge from the latest main branch when conflicts appear.
- Do not force-push unrelated history rewrites into an active review unless a
  maintainer asks for it.

## References We Borrow From

This guide is tailored to NomiFun, but it borrows proven practices from:

- [GitHub contributor guideline docs](https://docs.github.com/en/communities/setting-up-your-project-for-healthy-contributions/setting-guidelines-for-repository-contributors)
- [Open Source Guides: How to Contribute](https://opensource.guide/how-to-contribute/)
- [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)
- [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/about.html)
- [Kubernetes pull request process](https://www.kubernetes.dev/docs/guide/pull-requests/)
- [scikit-learn contributing guide](https://scikit-learn.org/stable/developers/contributing.html)
