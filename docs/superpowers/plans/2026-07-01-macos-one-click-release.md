# macOS One-Click Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `bun run release:mac` with the same APPEND/CREATE automation model as `release:win`.

**Architecture:** A macOS-only Bash orchestrator owns release decisions and side effects; a tiny Bun launcher mirrors the Windows launcher and enforces platform routing. A shell dry-run test uses stubbed `gh`/`git` commands to validate the CLI contract without building or publishing.

**Tech Stack:** Bash, Bun launcher script, GitHub CLI, existing `build:mac`, existing `make-latest-json.mjs`, shell test.

---

### Task 1: Dry-Run Contract Test

**Files:**
- Create: `scripts/test-release-mac-dry-run.sh`

- [ ] **Step 1: Write the failing shell test**

Create a test that:
- stubs `gh` so release existence can be toggled;
- stubs `git pull`;
- sets temporary updater/signing files via env overrides;
- verifies APPEND dry-run succeeds without build/upload;
- verifies CREATE dry-run without notes fails.

- [ ] **Step 2: Run the test and verify it fails**

Run: `bash scripts/test-release-mac-dry-run.sh`

Expected: FAIL because `scripts/release-mac.sh` does not exist yet.

### Task 2: macOS Release Orchestrator

**Files:**
- Create: `scripts/release-mac.sh`
- Create: `scripts/run-mac-release.mjs`

- [ ] **Step 1: Implement `release-mac.sh`**

Support:
- `-Version`, `-Notes`, `-NotesFile`, `-DryRun`, `-NoPush`, `-SkipPull`;
- automatic `APPEND` when `gh release view v<version>` succeeds;
- automatic `CREATE` otherwise;
- CREATE requires notes;
- APPEND uploads macOS assets and commits `latest.json`;
- CREATE can bump, commit, tag, push, create Release, and upload macOS assets;
- `-NoPush` matches Windows semantics;
- macOS release builds are signed/notarized through `build:mac --signed --config apps/desktop/tauri.updater.conf.json`.

- [ ] **Step 2: Implement `run-mac-release.mjs`**

Mirror `run-win-release.mjs`: reject non-macOS platforms, strip a lone `--`, and forward arguments to the Bash script.

- [ ] **Step 3: Run the dry-run test and verify it passes**

Run: `bash scripts/test-release-mac-dry-run.sh`

Expected: PASS with `release-mac dry-run: ok`.

### Task 3: Script Registry And Docs

**Files:**
- Modify: `package.json`
- Modify: `scripts/scripts.json`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `RELEASING.md`
- Modify: `RELEASING.zh-CN.md`

- [ ] **Step 1: Register `release:mac`**

Add `release:mac` to `package.json` and `scripts/scripts.json` next to `release:win`.

- [ ] **Step 2: Regenerate README script table**

Run: `bun run help --readme`.

- [ ] **Step 3: Update bilingual release docs**

Document the two modes, one-time setup, and examples:
- `bun run release:mac`
- `bun run release:mac -Version 0.1.14 -NotesFile notes.md`
- `bun run release:mac -DryRun`

### Task 4: Verification And Push

**Files:**
- All changed files above

- [ ] **Step 1: Run focused tests**

Run:
- `bash scripts/test-release-mac-dry-run.sh`
- `bun run release:mac -DryRun -SkipPull`
- `bun run help --check`

- [ ] **Step 2: Run broad check**

Run: `bun run check`

- [ ] **Step 3: Commit and push**

Commit message: `feat(release): add one-click macOS release`

Push `main` to `origin`.
