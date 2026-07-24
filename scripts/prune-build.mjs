#!/usr/bin/env bun
/**
 * prune-build -- self-cleaning preflight for every build/dev/test entry point.
 *
 * Keeps build artifact directories bounded WITHOUT a scheduled/cron job.
 * "Cleanup cadence = build cadence": this runs once at the START of every build,
 * before cargo touches anything, so the previous session's cruft is reclaimed
 * exactly when the next session begins. The growth driver IS the build, so
 * hooking cleanup to the build makes the two cadences match by construction.
 *
 * What it does (dev/test preflight, in order):
 *   1. GC the incremental cache PER UNIT: inside each incremental/<crate>-<hash>/
 *      dir, keep only the newest finalized session (the one rustc would load)
 *      and delete older/interrupted sessions + orphan locks. This preserves
 *      cross-session warmth (zero rebuild-speed regression) while removing the
 *      dead sessions that ballooned this to 82G/241k-files in 2 days.
 *   2. Remove leftover junk and stale debug WebUI resource staging.
 *   3. Size-gated cap: only if build.noindex / target exceed their soft cap,
 *      run `cargo sweep --maxsize` to trim the oldest artifacts. Without
 *      cargo-sweep, evict only the oldest incremental units to a fixed budget.
 *   4. Hard backstop: if build.noindex or target STILL exceeds CAP_GB, nuke
 *      debug/ + release/ intermediates (all-or-nothing on Windows for profile
 *      dirs; release bundles are preserved). This is the "can never silently
 *      balloon" guarantee.
 *
 * Release build split:
 *   --pre   cheap, output-cleaning preflight run by tauri's beforeBuildCommand
 *           BEFORE the release compile: drop the stale bundle (old installers) +
 *           junk. Runs in seconds, so cargo starts compiling immediately.
 *   --post  bounded cleanup AFTER a successful release build. It preserves the
 *           warm debug cache and freshly-built release bundle.
 *   --release  explicit destructive cleanup used by `bun run clean`: remove
 *           stale outputs and reclaim debug profiles + flycheck on demand.
 *
 * Design invariants:
 *   - NEVER fails the build chain (always exits 0).
 *   - Cross-platform (macOS / Linux / Windows): all paths via node:path, all
 *     deletes and logical-byte measurements use the same node:fs implementation.
 *     `cargo sweep` is optional. Without it, per-unit LRU eviction retains the
 *     newest incremental units; the hard backstop still bounds the directory.
 *   - Windows specifics (win32-only branches; mac/linux paths are untouched):
 *       * a running image LOCKS its .exe, so before any WHOLESALE profile delete
 *         we kill stale dev binaries (kill-stale-dev.mjs) to release locks;
 *       * wholesale deletes are ALL-OR-NOTHING: a lock-induced partial delete
 *         that pruned deps/ and .fingerprint/ to different extents could make
 *         cargo link a stale/missing dep, so on residue we retry then drop
 *         .fingerprint (forcing a loud recompile over a silent wrong build);
 *       * big trees are cleared with `robocopy` empty-mirror (fast, long-path-
 *         safe, /XJ so it never purges the D: cache-junction targets).
 *   - Fast in the normal case: GC + du checks only; cargo-sweep runs ONLY when
 *     a dir is genuinely over cap.
 *   - Safe: only ever deletes regenerable build artifacts, never source code.
 *     Only touches known Rust target triple dirs for supported desktop targets
 *     (Linux/macOS/Windows), and preserves release/bundle outputs when trimming
 *     release intermediates.
 *   - Idempotent: a second run in a row is a near no-op.
 *
 * Usage (always via package.json / tauri beforeBuildCommand, never by hand):
 *   bun scripts/prune-build.mjs             # dev/test preflight (GC + caps)
 *   bun scripts/prune-build.mjs --pre       # release pre-step (stale bundle + junk)
 *   bun scripts/prune-build.mjs --post      # release post-step (bounded cleanup)
 *   bun scripts/prune-build.mjs --release   # destructive cleanup (`bun run clean`)
 *   bun scripts/prune-build.mjs --cap 40    # override hard cap (GB)
 */
import { execSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync, statfsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BUILD_DIR = join(ROOT, 'build.noindex');
const TARGET_DIR = join(ROOT, 'target');
const isWin = process.platform === 'win32';
const KNOWN_TARGET_TRIPLE_RE = /^(x86_64|aarch64)-unknown-linux-gnu$|^(x86_64|aarch64)-pc-windows-msvc$|^(x86_64|aarch64|universal)-apple-darwin$/;

// ── Tunables ───────────────────────────────────────────────────────────────
// The full cross-platform dependency baseline is currently ~17G before the
// useful incremental cache. Leave room for one warm debug working set while
// bounding the old feature/target units that previously grew beyond 80G.
const BUILD_MAXSIZE_GB = 28;
const TARGET_MAXSIZE_GB = 5; // cargo-sweep trims target/ back under this
const BUILD_INCREMENTAL_BUDGET_GB = 8;
const TARGET_INCREMENTAL_BUDGET_GB = 2;

// ── Parse flags ──────────────────────────────────────────────────────────────
const args = process.argv.slice(2);
const isRelease = args.includes('--release');
const isPre = args.includes('--pre');
const isPost = args.includes('--post');
const capIdx = args.indexOf('--cap');
const CAP_GB = capIdx >= 0 && args[capIdx + 1] ? Number(args[capIdx + 1]) : 36;

const TAG = '[prune-build]';

function log(msg) {
  console.log(`${TAG} ${msg}`);
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/** Sum file sizes under a dir with a pure-Node walk. Cross-platform, no shell. */
function dirSizeBytes(dir) {
  let total = 0;
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    let ents;
    try { ents = readdirSync(d, { withFileTypes: true }); } catch { continue; }
    for (const ent of ents) {
      const p = join(d, ent.name);
      if (ent.isDirectory()) stack.push(p); // Dirent.isDirectory() is false for symlinks → no loops
      else { try { total += statSync(p).size; } catch { /* skip */ } }
    }
  }
  return total;
}

/**
 * Directory size in logical GB. Using the same file-size walk on every platform
 * avoids comparing Unix allocated blocks (`du`) with Windows logical bytes.
 */
function dirSizeGB(dir) {
  if (!existsSync(dir)) return 0;
  try { return dirSizeBytes(dir) / 1024 ** 3; } catch { return 0; }
}

/** Format GB for display. */
function fmtGB(gb) {
  if (gb < 1) return `${(gb * 1024).toFixed(0)}M`;
  return `${gb.toFixed(1)}G`;
}

function relPath(p) {
  return p.startsWith(ROOT) ? p.slice(ROOT.length + 1) : p;
}

/** Known <root>/<triple>/ dirs that this project may create for desktop builds. */
function knownTripleDirs(rootDir) {
  if (!existsSync(rootDir)) return [];
  try {
    return readdirSync(rootDir, { withFileTypes: true })
      .filter((ent) => ent.isDirectory() && KNOWN_TARGET_TRIPLE_RE.test(ent.name))
      .map((ent) => ({ name: ent.name, dir: join(rootDir, ent.name) }));
  } catch {
    return [];
  }
}

function knownTargetTripleDirs() {
  return knownTripleDirs(TARGET_DIR);
}

function incrementalCacheDirsUnder(rootDir) {
  const dirs = [join(rootDir, 'debug', 'incremental'), join(rootDir, 'release', 'incremental')];
  for (const { dir } of knownTripleDirs(rootDir)) {
    dirs.push(join(dir, 'debug', 'incremental'));
    dirs.push(join(dir, 'release', 'incremental'));
  }
  return dirs;
}

/** Incremental cache dirs across default and known target-triple profiles. */
function incrementalCacheDirs() {
  return [...incrementalCacheDirsUnder(BUILD_DIR), ...incrementalCacheDirsUnder(TARGET_DIR)];
}

/** Remove stale bundle outputs across default and known target-triple release dirs. */
function removeStaleBundleDirs() {
  rmDir(join(TARGET_DIR, 'release', 'bundle'), 'target/release/bundle (stale installers)');
  for (const { name, dir } of knownTargetTripleDirs()) {
    rmDir(join(dir, 'release', 'bundle'), `target/${name}/release/bundle (stale installers)`);
  }
}

/**
 * Remove Tauri's regenerable WebUI resource staging. The legacy `_up_` layout
 * accumulated old Vite hashes; production now maps the resource to a stable
 * `webui-dist` path, while dev serves Vite directly and disables the resource.
 */
function removeStaleWebUiResourceDirs(profiles) {
  for (const rootDir of [BUILD_DIR, TARGET_DIR]) {
    const roots = [{ name: '', dir: rootDir }, ...knownTripleDirs(rootDir)];
    for (const { name, dir } of roots) {
      for (const profile of profiles) {
        const profileDir = join(dir, profile);
        const prefix = name ? `${name}/${profile}` : profile;
        rmDir(
          join(profileDir, 'webui-dist'),
          `${relPath(rootDir)}/${prefix}/webui-dist (stale resource stage)`,
        );
        rmDir(
          join(profileDir, '_up_', '_up_', 'ui', 'dist'),
          `${relPath(rootDir)}/${prefix}/_up_/_up_/ui/dist (legacy resource stage)`,
        );
      }
    }
  }
}

/**
 * Clear release intermediates while preserving release/bundle. This protects
 * freshly built installers and updater signatures while reclaiming deps/build/
 * .fingerprint/incremental/root binaries that cargo can regenerate.
 */
function clearReleaseIntermediatesPreservingBundle(releaseDir, label) {
  if (!existsSync(releaseDir)) return;
  let removed = 0;
  try {
    for (const ent of readdirSync(releaseDir, { withFileTypes: true })) {
      if (ent.name === 'bundle') continue;
      rmSync(join(releaseDir, ent.name), { recursive: true, force: true });
      removed++;
    }
    if (removed > 0) log(`  removed ${label} intermediates (kept release/bundle)`);
  } catch (e) {
    log(`  WARN: could not clear ${label} intermediates: ${e.message}`);
  }
}

/**
 * Windows: empty-mirror a scratch dir over `dir` with robocopy, then the caller
 * drops the emptied shell. robocopy is the fastest reliable way to clear huge
 * many-file trees on Windows and is long-path-safe (deep NTFS paths that choke
 * rmSync). Flags: /XJ — do NOT descend junctions (an empty mirror would else
 * purge the junction TARGET's real contents; this repo junctions caches onto D:);
 * /R:0 /W:0 — never retry/wait on a locked file (leave it; caller detects residue).
 * robocopy exit codes: 1/2/3 == SUCCESS, >=8 == failure, status null == missing.
 * Returns true iff robocopy ran without a hard error (status < 8).
 */
function robocopyEmptyMirror(dir) {
  let scratch;
  try {
    scratch = mkdtempSync(join(tmpdir(), 'nomi-empty-'));
    const r = spawnSync(
      'robocopy',
      [scratch, dir, '/MIR', '/XJ', '/R:0', '/W:0', '/MT:16', '/NFL', '/NDL', '/NJH', '/NJS', '/NC', '/NS', '/NP'],
      { stdio: 'ignore', timeout: 120_000 },
    );
    return r.status !== null && r.status < 8;
  } catch {
    return false;
  } finally {
    if (scratch) { try { rmSync(scratch, { recursive: true, force: true }); } catch { /* ignore */ } }
  }
}

/** Remove a directory tree. Silent on failure. Lock-resilient on Windows. */
function rmDir(dir, label) {
  if (!existsSync(dir)) return;
  try {
    // Windows: empty the tree with robocopy first (fast + long-path-safe), then
    // rmSync drops the emptied shell. If robocopy is absent, rmSync alone.
    if (isWin) robocopyEmptyMirror(dir);
    rmSync(dir, { recursive: true, force: true });
    log(`  removed ${label || dir}`);
  } catch (e) {
    log(`  WARN: could not remove ${label || dir}: ${e.message}`);
  }
}

/**
 * Kill leftover dev binaries that lock files under target/ or build.noindex/.
 * Windows only: a running image locks its .exe, so a wholesale delete would
 * otherwise be a no-op or a torn partial. Reuses the battle-tested
 * kill-stale-dev.mjs (taskkill /T tree-kill + lock-release sleep). Best-effort.
 * Called ONLY before wholesale-profile deletes — never during routine per-unit
 * incremental GC (those are independent units, non-fatal on a locked file).
 */
function killStaleLockers() {
  if (!isWin) return;
  try {
    execSync(`"${process.execPath}" "${join(ROOT, 'scripts', 'kill-stale-dev.mjs')}"`, {
      stdio: 'ignore',
      timeout: 30_000,
    });
  } catch { /* best effort — must never block the build */ }
}

/**
 * All-or-nothing wholesale delete of a build profile dir (e.g. build.noindex/
 * debug). Hazard on Windows: a held lock makes the delete PARTIAL, and a tree
 * where deps/*.rlib and .fingerprint/ were pruned to different extents can make
 * cargo link a stale/missing dep — a WRONG build, not just a cold one. The
 * caller kills lockers first; here we delete, and on residue retry once (after
 * another kill), then as a last resort drop .fingerprint so cargo cannot trust
 * the torn deps/ (a loud recompile beats a silent wrong build) and warn loudly.
 */
function nukeProfileAllOrNothing(profileDir, label) {
  if (!existsSync(profileDir)) return;
  rmDir(profileDir, label);
  if (!existsSync(profileDir)) return; // fully removed
  killStaleLockers();
  rmDir(profileDir, `${label} (retry)`);
  if (!existsSync(profileDir)) return;
  try { rmSync(join(profileDir, '.fingerprint'), { recursive: true, force: true }); } catch { /* ignore */ }
  log(`  WARN: ${label} only partially removed (a process still holds a lock).`);
  log('  WARN: close the dev app / editor and rebuild; run `cargo clean` if the build errors.');
}

/**
 * Cheap pre-release preflight (tauri beforeBuildCommand): drop the stale bundle
 * (old installers from a previous build/version) + leftover junk, so the produced
 * dist is clean. Runs in seconds and touches NOTHING the compile needs, so cargo
 * starts immediately. The bounded cache pass runs via --post after the build.
 */
function preReleaseClean() {
  log('pre-release: dropping stale bundle + junk (fast — compile starts now)...');
  removeStaleBundleDirs();
  removeStaleWebUiResourceDirs(['release']);
  rmGlob(BUILD_DIR, /^_.*\.(log|json|out|err)$/, 'leftover log/json');
  rmDir(join(BUILD_DIR, 'tmp'), 'build.noindex/tmp');
}

/**
 * Explicit on-demand reclaim of debug profiles via `bun run clean`. Normal and
 * post-release cleanup never call this because preserving the warm debug profile
 * is essential for fast edit/build cycles. Release intermediates are untouched.
 */
function reclaimDebugDeadWeight() {
  log('reclaiming debug dead weight (debug profile + flycheck)...');
  killStaleLockers(); // release locks before wholesale deletes (Windows)
  nukeProfileAllOrNothing(join(BUILD_DIR, 'debug'), 'build.noindex/debug');
  for (const { name, dir } of knownTripleDirs(BUILD_DIR)) {
    nukeProfileAllOrNothing(join(dir, 'debug'), `build.noindex/${name}/debug`);
  }
  nukeProfileAllOrNothing(join(TARGET_DIR, 'debug'), 'target/debug');
  for (const { name, dir } of knownTargetTripleDirs()) {
    nukeProfileAllOrNothing(join(dir, 'debug'), `target/${name}/debug`);
  }
  rmDir(join(TARGET_DIR, 'flycheck0'), 'target/flycheck0');
}

/** Is cargo-sweep on PATH? Its --maxsize cap is warn-only (a no-op) without it. */
function cargoSweepInstalled() {
  try {
    execSync(isWin ? 'where cargo-sweep' : 'command -v cargo-sweep', { stdio: 'ignore' });
    return true;
  } catch { return false; }
}

/** Warn (never auto-delete unrelated files) when the build drive is running low. */
function freeSpaceWarn() {
  try {
    const s = statfsSync(ROOT);
    const freeGB = (s.bsize * s.bavail) / 1024 ** 3;
    const warnGB = isWin ? 50 : 20;
    if (freeGB < warnGB) {
      log(`WARN: only ${fmtGB(freeGB)} free on the build drive — run 'bun run clean' to reclaim, or 'cargo clean'`);
    }
  } catch { /* statfsSync unavailable — skip */ }
}

/** Remove files matching a pattern in a directory (non-recursive). */
function rmGlob(dir, pattern, label) {
  if (!existsSync(dir)) return;
  let count = 0;
  try {
    for (const entry of readdirSync(dir)) {
      if (!pattern.test(entry)) continue;
      const full = join(dir, entry);
      try {
        if (statSync(full).isFile()) {
          rmSync(full, { force: true });
          count++;
        }
      } catch { /* skip */ }
    }
    if (count > 0) log(`  removed ${count} ${label} files`);
  } catch { /* dir might have vanished */ }
}

/**
 * Per-unit incremental GC.
 *
 * Layout: incremental/<crate>-<hash>/s-<id>-<svh>/  (+ a 0-byte s-<id>.lock)
 * rustc loads only the newest *finalized* session per unit; older sessions and
 * leftover "-working" dirs from interrupted compiles are dead and never GC'd by
 * cargo during fast dev iteration. We keep the newest finalized session per unit
 * (warmth preserved) and delete the rest. NEVER groups across units — every
 * <crate>-<hash> dir (incl. each per-crate build_script_build-*) is independent.
 */
function pruneIncrementalSessions(incrDir) {
  let units;
  try { units = readdirSync(incrDir); } catch { return; }

  let pruned = 0;
  for (const unit of units) {
    const unitPath = join(incrDir, unit);
    try { if (!statSync(unitPath).isDirectory()) continue; } catch { continue; }

    let entries;
    try { entries = readdirSync(unitPath); } catch { continue; }

    const sessions = [];
    for (const e of entries) {
      if (!e.startsWith('s-')) continue;
      const p = join(unitPath, e);
      try {
        const s = statSync(p);
        if (s.isDirectory()) {
          sessions.push({ name: e, mtime: s.mtimeMs, working: e.endsWith('-working') });
        }
      } catch { /* skip */ }
    }
    if (sessions.length === 0) continue;

    // Prefer the newest finalized session; fall back to newest overall.
    const finals = sessions.filter((s) => !s.working);
    const pool = (finals.length ? finals : sessions).sort((a, b) => b.mtime - a.mtime);
    const keptDir = pool[0].name;
    // Session dir is s-<id>-<svh>-<random>; its lock file is s-<id>-<svh>.lock.
    // Strip the trailing "-<random>" segment to recover the lock name.
    const keptLock = `${keptDir.replace(/-[^-]+$/, '')}.lock`;

    for (const e of entries) {
      if (e === keptDir || e === keptLock) continue;
      try {
        rmSync(join(unitPath, e), { recursive: true, force: true });
        pruned++;
      } catch { /* best effort */ }
    }
  }
  if (pruned > 0) log(`  GC'd ${pruned} stale incremental entries (kept newest session per unit)`);
}

/**
 * Bound incremental storage without destroying the whole warm cache. Each
 * `<crate>-<hash>` directory is an independent Cargo unit; evicting the oldest
 * units retains recently edited crates and removes obsolete feature/target
 * fingerprints consistently on Windows, macOS, and Linux.
 */
function trimIncrementalUnitsToBudget(incrDirs, maxGB, label) {
  const units = [];
  let totalBytes = 0;
  for (const incrDir of incrDirs) {
    let entries;
    try { entries = readdirSync(incrDir, { withFileTypes: true }); } catch { continue; }
    for (const ent of entries) {
      if (!ent.isDirectory()) continue;
      const unitPath = join(incrDir, ent.name);
      try {
        const stat = statSync(unitPath);
        const bytes = dirSizeBytes(unitPath);
        totalBytes += bytes;
        units.push({ path: unitPath, bytes, mtime: stat.mtimeMs });
      } catch { /* unit disappeared or is locked */ }
    }
  }

  const maxBytes = maxGB * 1024 ** 3;
  if (totalBytes <= maxBytes) return;
  units.sort((a, b) => a.mtime - b.mtime);
  let removed = 0;
  let freedBytes = 0;
  for (const unit of units) {
    if (totalBytes - freedBytes <= maxBytes) break;
    try {
      rmSync(unit.path, { recursive: true, force: true });
      removed++;
      freedBytes += unit.bytes;
    } catch { /* best effort; the hard cap remains the backstop */ }
  }
  log(
    `  trimmed ${removed} LRU ${label} units; `
      + `${fmtGB(totalBytes / 1024 ** 3)} -> ${fmtGB((totalBytes - freedBytes) / 1024 ** 3)}`,
  );
}

/**
 * Trim a target dir back under maxGB using cargo-sweep --maxsize (removes oldest
 * artifacts first). Only call when the dir is actually over cap. Never fatal.
 */
function cargoSweepMaxsize(targetDir, maxGB, label) {
  if (!existsSync(targetDir)) return;
  try {
    const env = { ...process.env, CARGO_TARGET_DIR: targetDir, CARGO_NET_OFFLINE: 'true' };
    execSync(`cargo sweep --maxsize ${maxGB}GB "${ROOT}"`, {
      encoding: 'utf8',
      stdio: 'pipe',
      env,
      timeout: 60_000,
    });
    log(`  capped ${label} at ${maxGB}GB`);
  } catch (e) {
    log(`  WARN: cargo sweep --maxsize on ${label} failed (${e.message?.split('\n')[0] || 'cargo-sweep missing?'})`);
  }
}

function boundedCleanup(initialSizes) {
  for (const incrDir of incrementalCacheDirs()) {
    if (!existsSync(incrDir)) continue;
    pruneIncrementalSessions(incrDir);
  }

  // Dev serves Vite directly, so copied production resources are dead weight.
  removeStaleWebUiResourceDirs(['debug']);
  rmGlob(BUILD_DIR, /^_.*\.(log|json|out|err)$/, 'leftover log/json');
  rmDir(join(BUILD_DIR, 'tmp'), 'build.noindex/tmp');

  const haveSweep = cargoSweepInstalled();
  if (initialSizes.buildGB > BUILD_MAXSIZE_GB) {
    if (haveSweep) cargoSweepMaxsize(BUILD_DIR, BUILD_MAXSIZE_GB, 'build.noindex');
    else trimIncrementalUnitsToBudget(
      incrementalCacheDirsUnder(BUILD_DIR),
      BUILD_INCREMENTAL_BUDGET_GB,
      'build.noindex incremental',
    );
  }
  if (initialSizes.targetGB > TARGET_MAXSIZE_GB) {
    if (haveSweep) cargoSweepMaxsize(TARGET_DIR, TARGET_MAXSIZE_GB, 'target');
    else trimIncrementalUnitsToBudget(
      incrementalCacheDirsUnder(TARGET_DIR),
      TARGET_INCREMENTAL_BUDGET_GB,
      'target incremental',
    );
  }

  // One post-clean measurement per root serves both the hard cap and final log.
  // This avoids repeatedly walking hundreds of thousands of files on Windows.
  let buildGB = dirSizeGB(BUILD_DIR);
  let targetGB = dirSizeGB(TARGET_DIR);
  if (buildGB > CAP_GB) {
    log(`WARN: build.noindex is ${fmtGB(buildGB)} > cap ${CAP_GB}G - nuking profiles as last resort`);
    killStaleLockers();
    nukeProfileAllOrNothing(join(BUILD_DIR, 'debug'), 'build.noindex/debug (cap exceeded)');
    nukeProfileAllOrNothing(join(BUILD_DIR, 'release'), 'build.noindex/release (cap exceeded)');
    for (const { name, dir } of knownTripleDirs(BUILD_DIR)) {
      nukeProfileAllOrNothing(join(dir, 'debug'), `build.noindex/${name}/debug (cap exceeded)`);
      nukeProfileAllOrNothing(join(dir, 'release'), `build.noindex/${name}/release (cap exceeded)`);
    }
    buildGB = dirSizeGB(BUILD_DIR);
  }

  if (targetGB > CAP_GB) {
    log(`WARN: target is ${fmtGB(targetGB)} > cap ${CAP_GB}G - reclaiming profiles as last resort`);
    killStaleLockers();
    nukeProfileAllOrNothing(join(TARGET_DIR, 'debug'), 'target/debug (cap exceeded)');
    clearReleaseIntermediatesPreservingBundle(join(TARGET_DIR, 'release'), 'target/release (cap exceeded)');
    for (const { name, dir } of knownTargetTripleDirs()) {
      nukeProfileAllOrNothing(join(dir, 'debug'), `target/${name}/debug (cap exceeded)`);
      clearReleaseIntermediatesPreservingBundle(join(dir, 'release'), `target/${name}/release (cap exceeded)`);
    }
    targetGB = dirSizeGB(TARGET_DIR);
  }
  return { buildGB, targetGB };
}

// ── Main ─────────────────────────────────────────────────────────────────────
try {
  const beforeSizes = { buildGB: dirSizeGB(BUILD_DIR), targetGB: dirSizeGB(TARGET_DIR) };
  const beforeGB = beforeSizes.buildGB + beforeSizes.targetGB;
  const mode = isPre ? ' [pre]' : isPost ? ' [post]' : isRelease ? ' [release]' : '';
  log(`start: ${fmtGB(beforeGB)} total (build.noindex + target)${mode}`);
  let afterSizes;

  if (isPre) {
    // Cheap pre-build step (tauri beforeBuildCommand): clean output + junk only,
    // so the release compile starts immediately. Bounded cleanup runs via --post.
    preReleaseClean();
  } else if (isPost) {
    // Preserve warm debug caches after release while still enforcing disk bounds.
    afterSizes = boundedCleanup(beforeSizes);
  } else if (isRelease) {
    // Explicit destructive cleanup (`bun run clean`): outputs + debug profiles.
    preReleaseClean();
    reclaimDebugDeadWeight();
  } else {
    afterSizes = boundedCleanup(beforeSizes);
  }

  afterSizes ??= { buildGB: dirSizeGB(BUILD_DIR), targetGB: dirSizeGB(TARGET_DIR) };
  const afterGB = afterSizes.buildGB + afterSizes.targetGB;
  const freed = beforeGB - afterGB;
  log(freed > 0.01 ? `done: freed ${fmtGB(freed)} (${fmtGB(beforeGB)} -> ${fmtGB(afterGB)})` : `done: ${fmtGB(afterGB)} total (already clean)`);
  freeSpaceWarn();
} catch (e) {
  // NEVER fail the build chain.
  log(`WARN: prune failed (${e.message}) — continuing build`);
}

process.exit(0);
