# Companion Creation Visibility Repair Design

## Proven root cause

A fresh companion is validly created with `model: null`. The persisted data and
the create API are not failing. The observed disappearance came from serving an
older ignored `ui/dist` bundle against a newer backend contract: that bundle
dereferenced `profile.model.provider_id`, threw during roster rendering, and was
caught by the route error boundary. The create success toast ran before the
roster refresh callback, so users saw success immediately before the UI failed.

The development topology made the skew easy to trigger. Vite on port 5173
served current source, while the backend port 8787 could also fall back to a
previous production bundle. Reopening 8787 therefore reproduced a bug that was
already fixed in source. Existing companion data remained intact throughout.

## Goals

- Render a fresh `model: null` companion without throwing.
- Never serve a static SPA unless it is the exact UI build paired with the Rust
  host.
- Make WebUI development expose one UI, from Vite, while the Rust process is API
  only.
- Report creation success only after the roster is visible, while accurately
  distinguishing a completed create from a failed refresh.
- Reject stale or legacy build artifacts with an actionable rebuild error.

## Non-goals and compatibility boundary

- Do not migrate, rewrite, or synthesize model configuration in existing
  companion data. `model: null` remains a supported fresh-companion state.
- Do not add a compatibility reader for old or partial build manifests. Static
  serving requires the new strict manifest; old bundles must be rebuilt.
- Do not infer freshness from file timestamps, semantic version alone, or a
  best-effort warning.
- Do not change companion IDs, persistence formats, or create API semantics.

## Options considered

### 1. UI null fix and regression test only

This is the smallest source repair and directly prevents the null dereference.
It does not prevent a backend or operator from serving an older ignored
`ui/dist`, so the same class of incident remains possible.

### 2. App-version or API-version handshake only

Comparing `app_version` or an integer API contract catches deliberate version
bumps. It cannot distinguish two UI builds with the same version, and a legacy
bundle may not contain client-side handshake logic at all. It is useful as one
manifest field, but insufficient as the freshness guarantee.

### 3. Strict manifest, exact build pairing, and API-only development

This is the selected design. Every Vite production build emits one strict
manifest with a new random build identifier. The Rust host captures that exact
identifier when it is built, and static serving validates equality before
mounting the SPA. Development starts the Rust host with `--api-only`, so port
8787 cannot expose a stale SPA and port 5173 is the sole WebUI.

The trade-off is intentional build ordering: UI must be built before a static
host. Missing or mismatched artifacts now fail fast and require a rebuild
instead of silently continuing.

## Selected design

### Strict build manifest

`ui/vite.config.ts` emits `ui/dist/nomifun-build.json`:

```json
{
  "schema": 1,
  "app_version": "0.2.20",
  "api_contract_version": 1,
  "frontend_build_id": "7e9b8f1c-1dc7-4d97-8f3e-65c6f564eaf2"
}
```

The accepted shape has exactly these four fields. `app_version` comes from the
UI package, `api_contract_version` comes from the repository-root
`ui-api-contract-version.txt`, and `frontend_build_id` is minted once per Vite
build with `randomUUID()`.

The shared Rust build helper validates the exact field set, schema, app version,
API contract, and non-empty build ID, then embeds the ID as
`NOMIFUN_FRONTEND_BUILD_ID`. Static-host startup validates `index.html`, parses
the manifest with unknown fields denied, repeats the version checks, and
requires exact build-ID equality. A missing legacy manifest, extra field, or
mismatch is a startup error that tells the operator to run `bun run build:ui`.

Desktop and Docker preserve the same ordering: build/copy the UI manifest before
compiling the Rust host that captures it. Debug builds ignore `ui/dist`
completely unless the Web host's explicit `static-webui` Cargo feature is
enabled, so an old ignored artifact cannot break API-only/Vite development.
Conversely, static Web startup requires an embedded build ID and fails before
runtime initialization when the host was built without that feature. This is
not a compatibility fallback.

### Development and production entry points

`dev:web` starts `nomifun-web --api-only` and Vite. API, auth, health, and
WebSocket paths remain proxied from 5173 to 8787, but the backend has no SPA
fallback. `serve:web` builds the UI first, then compiles with
`--features static-webui` and starts static mode with `--dist ui/dist`. Workspace
test entry points run `ensure-ui-dist.mjs` so Tauri's resource preflight also
works on a clean clone. Docker explicitly carries `nomifun-build.json` from the
UI stage into the Rust build and runtime image.

### Truthful companion-creation feedback

The rail treats model readiness as `profile.model !== null`. After the create
POST succeeds, it closes the modal to prevent a duplicate submission and awaits
`onCreated`. The page callback awaits `companionsApi.refresh()` so the new row is
present before success is announced; `shared.refresh()` remains non-blocking.

If roster refresh fails, the companion is already persisted. That error is
caught around `onCreated` and shown as a warning containing the translated
“created” message plus the original refresh error. It does not fall into the
generic create failure branch. The normal success toast appears only after the
awaited roster refresh succeeds.

## Test strategy

- A Bun SSR regression renders the real rail with a complete
  `ICompanionWithStatus` fixture whose `model` is `null`.
- Source/ordering regressions require `await onCreated(profile)` before
  `Message.success` and require the refresh-specific warning branch.
- Bun build tests inspect the emitted manifest, dev API-only command, production
  build order, and Docker manifest copy.
- Rust tests reject missing/invalid manifests, unknown fields, schema/app/API
  mismatches, blank IDs, and exact-ID mismatches; they accept only the paired
  manifest.
- Focused tests, UI typecheck, Rust package tests, formatting, and diff checks
  form the completion gate.
