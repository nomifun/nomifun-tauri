# Companion Creation Visibility Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make fresh companions immediately visible and prevent any Rust static host from serving a UI build other than the exact manifest-paired artifact.

**Architecture:** Vite emits a strict four-field manifest with a per-build UUID and a repository-owned API contract version. Rust hosts validate and embed that UUID at build time, static WebUI startup requires exact equality, and Web development runs the backend API-only. The companion rail renders `model: null` and delays truthful success feedback until the roster refresh completes.

**Tech Stack:** React 19, TypeScript, Vite, Bun test runner, Rust, Axum, Tauri, Docker.

## Global Constraints

- `model: null` is a valid fresh-companion state; do not synthesize a model.
- The manifest contains exactly `schema`, `app_version`, `api_contract_version`, and `frontend_build_id`.
- `schema` is `1`; `api_contract_version` is the positive integer in `ui-api-contract-version.txt`.
- `frontend_build_id` is a UUID v4 minted once per Vite build and compared by exact equality.
- Static mode rejects missing, legacy, malformed, extra-field, or mismatched manifests; it never falls back to a warning.
- `dev:web` exposes UI only through Vite and runs `nomifun-web --api-only`.
- Debug/API-only builds ignore `ui/dist`; debug static mode requires the explicit `static-webui` Cargo feature.
- A host without an embedded exact build ID must reject static mode before runtime initialization.
- Do not migrate existing companion data or support old build-manifest shapes.
- Leave all changes uncommitted.

---

### Task 1: Lock fresh-companion rendering and truthful feedback

**Files:**
- Modify: `ui/src/renderer/pages/nomi/CompanionSessionRail.test.ts`
- Modify: `ui/src/renderer/pages/nomi/CompanionSessionRail.tsx`
- Modify: `ui/src/renderer/pages/nomi/index.tsx`

**Interfaces:**
- Consumes: `ICompanionWithStatus`, `ICompanionProfile`, and `companionsApi.refresh()`.
- Produces: `onCreated: (profile: ICompanionProfile) => void | Promise<void>` and an awaited roster refresh before success.

- [ ] **Step 1: Add the failing rail regressions**

Use `renderToStaticMarkup(React.createElement(CompanionSessionRail, ...))` with a complete companion fixture containing `model: null`, and assert the rendered HTML contains its name. Add source-order assertions requiring:

```ts
const refresh = source.indexOf('await onCreated(profile)');
const success = source.indexOf("Message.success(t('nomi.companions.created'");
expect(refresh).toBeGreaterThan(-1);
expect(refresh).toBeLessThan(success);
```

Also require a nested `catch (refreshError)` with `Message.warning`, the
translated created message, and `String(refreshError)` before the generic
`catch (e)` branch.

- [ ] **Step 2: Verify RED**

Run:

```bash
bun test ui/src/renderer/pages/nomi/CompanionSessionRail.test.ts
```

Expected: the real `model: null` render passes on repaired source, while legacy
ordering fails because `await onCreated(profile)` and the refresh-specific catch
are absent.

- [ ] **Step 3: Implement the minimal creation sequence**

In `CompanionSessionRail.tsx`, allow an async callback, close the modal after the
POST, and isolate refresh failure from create failure:

```tsx
onCreated: (profile: ICompanionProfile) => void | Promise<void>;

setModalVisible(false);
try {
  await onCreated(profile);
} catch (refreshError) {
  Message.warning(
    `${t('nomi.companions.created', { companionName: profile.name })}: ${String(refreshError)}`
  );
  return;
}
Message.success(t('nomi.companions.created', { companionName: profile.name }));
```

In `index.tsx`, make `handleCreated` async, `await companionsApi.refresh()`, keep
`void shared.refresh()`, and update the companion/tab query only after the
roster refresh resolves.

- [ ] **Step 4: Verify GREEN**

Run the focused Bun command again. Expected: all rail tests pass, including the
real SSR null-model case and both ordering branches.

### Task 2: Generate and test the strict UI build manifest

**Files:**
- Create: `ui-api-contract-version.txt`
- Modify: `ui/vite.config.ts`
- Create: `scripts/check-ui-build-manifest.test.ts`
- Modify: `package.json`
- Modify: `Dockerfile`

**Interfaces:**
- Consumes: `ui/package.json` version and `ui-api-contract-version.txt`.
- Produces: `ui/dist/nomifun-build.json` with schema `1` and one UUID v4 build ID.

- [ ] **Step 1: Write the failing Bun contract tests**

Require the generated manifest to contain exactly the four selected fields,
match the UI version and contract file, and use this UUID-v4 pattern:

```ts
/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
```

In the same test file, require `dev:web` to contain `--api-only`, require
`serve:web` to run `bun run build:ui` before `cargo run -p nomifun-web`, and
require Docker to copy `nomifun-build.json` from the UI stage.

- [ ] **Step 2: Verify RED**

Run:

```bash
bun test scripts/check-ui-build-manifest.test.ts
```

Expected: fail because the manifest, API-only flag, production ordering, or
Docker copy is missing.

- [ ] **Step 3: Emit the manifest from Vite**

Set `ui-api-contract-version.txt` to `1`. Add a typed Vite build plugin that
validates the file as a positive safe integer, reads the UI package version,
calls `randomUUID()` once, and emits `nomifun-build.json` from
`generateBundle()` via `this.emitFile({ type: 'asset', ... })`.

- [ ] **Step 4: Separate development and production entry points**

Change `dev:web` to pass `--api-only`. Change `serve:web` to build the UI before
starting `nomifun-web --features static-webui -- --dist ui/dist`. Run
`ensure-ui-dist.mjs` before both workspace test entry points so clean-clone
desktop tests satisfy Tauri's resource preflight. Ensure the Docker Rust build
and runtime stages receive the UI-stage manifest before host compilation and
serving.

- [ ] **Step 5: Build and verify GREEN**

Run:

```bash
bun run build:ui
bun test scripts/check-ui-build-manifest.test.ts
```

Expected: Vite emits the manifest and all Bun manifest/pipeline assertions pass.

### Task 3: Enforce strict manifest validation in Rust

**Files:**
- Create: `crates/backend/nomifun-app/src/bootstrap/webui_dist.rs`
- Modify: `crates/backend/nomifun-app/src/bootstrap/mod.rs`
- Create: `apps/build-support/ui_build_manifest.rs`
- Create: `apps/web/build.rs`
- Modify: `apps/web/Cargo.toml`
- Modify: `apps/web/src/main.rs`
- Modify: `apps/desktop/build.rs`
- Modify: `apps/desktop/Cargo.toml`
- Modify: `apps/desktop/src/main.rs`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `nomifun-build.json`, `ui-api-contract-version.txt`, and the host package version.
- Produces: `validate_webui_dist(dist, expected_app_version, expected_build_id)` and build-time `NOMIFUN_FRONTEND_BUILD_ID`.

- [ ] **Step 1: Write validator tests first**

Cover acceptance of one exact manifest and rejection of each independent
failure: missing `index.html`, missing/invalid manifest, unknown field, wrong
schema, wrong app version, wrong API contract, blank build ID, and exact build-ID
mismatch. Add a `nomifun-web` parser test proving `--api-only` sets the flag.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p nomifun-app webui_dist
cargo test -p nomifun-web api_only_flag_disables_static_spa_mode
```

Expected: fail until the validator module and `--api-only` argument exist.

- [ ] **Step 3: Implement the shared validator**

Define `UiBuildManifest` with `#[serde(deny_unknown_fields)]`, the four exact
field types, schema constant `1`, and an `include_str!` reader for the root API
contract. Return actionable errors ending with the instruction to run
`bun run build:ui` and restart the backend.

- [ ] **Step 4: Embed the exact frontend build ID**

In `apps/build-support/ui_build_manifest.rs`, parse the same four-field shape,
verify the exact key set and versions, then emit:

```rust
println!("cargo:rustc-env=NOMIFUN_FRONTEND_BUILD_ID={build_id}");
```

Call the helper from both host `build.rs` files. Release builds always require
the manifest. Debug builds ignore it unless Web's `static-webui` feature is
enabled, which prevents an old ignored dist from blocking API-only/Vite
development. Web static startup must reject a binary with no embedded ID.

- [ ] **Step 5: Gate WebUI static mounting**

In `nomifun-web`, skip all dist validation and `ServeDir` mounting when
`args.api_only` is true. Otherwise validate the dist against
`env!("CARGO_PKG_VERSION")` and the embedded exact build ID before adding the SPA
fallback. Log `static_mode` as `api-only` or `spa`.

- [ ] **Step 6: Verify GREEN**

Run the two focused Cargo commands from Step 2 again. Expected: all validator
and API-only tests pass.

### Task 4: Run the completion gate and inspect scope

**Files:**
- Verify: all files listed in Tasks 1–3
- Verify: `docs/superpowers/specs/2026-07-16-companion-creation-visibility-repair-design.md`
- Verify: `docs/superpowers/plans/2026-07-16-companion-creation-visibility-repair.md`

**Interfaces:**
- Consumes: the completed repair.
- Produces: fresh test evidence and an uncommitted, scope-clean diff.

- [ ] **Step 1: Run frontend verification**

```bash
bun test ui/src/renderer/pages/nomi/CompanionSessionRail.test.ts
bun run build:ui
bun test scripts/check-ui-build-manifest.test.ts
bun run --filter=./ui typecheck
```

Expected: every command exits `0`; the rail suite includes the null-model and
truthful-toast regressions, and the manifest suite validates the newly built
artifact.

- [ ] **Step 2: Run Rust verification**

```bash
cargo test -p nomifun-app webui_dist
cargo test -p nomifun-web
cargo test -p nomifun-web --features static-webui
cargo check -p nomifun-desktop
cargo fmt --check
```

Expected: every command exits `0` with no failed tests or formatting diff.

- [ ] **Step 3: Check documentation and diff integrity**

```bash
rg -n 'TB[D]|TO[D]O|FIXM[E]|implement late[r]|fill i[n]' docs/superpowers/specs/2026-07-16-companion-creation-visibility-repair-design.md docs/superpowers/plans/2026-07-16-companion-creation-visibility-repair.md
git diff --check
git status --short
```

Expected: the placeholder scan has no matches, `git diff --check` exits `0`, and
status contains only the intended repair and documentation files. Do not stage
or commit the changes.
