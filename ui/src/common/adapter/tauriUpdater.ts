/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

/**
 * Tauri-native in-app updater adapter. Backs the ipcBridge `update` /
 * `autoUpdate` channels with `@tauri-apps/plugin-updater` (+ `plugin-process`
 * for relaunch). Mirrors tauriShell.ts: every call is GUARDED by `isTauriRuntime()`
 * and the Tauri modules load via dynamic `import()` so the WebUI browser bundle
 * never evaluates Tauri IPC code.
 *
 * Lifecycle — one shared `Update` resource flows across check → download →
 * install. `check()` returns a Rust-side Resource handle (`rid`); the SAME
 * object must be reused for `download()`/`install()`, so it is held in
 * `pendingUpdate` between calls. The modal performs two back-to-back checks
 * (autoUpdate.check then update.check); `checkPromise` memoizes them into ONE
 * network round-trip, while `force` re-checks on retry / modal reopen.
 *
 * The updater compares against the running bundle's version (Tauri reads it from
 * the workspace `Cargo.toml`, the single source of truth) and fetches the signed
 * `latest.json` from `plugins.updater.endpoints` in tauri.conf.json, verifying
 * each artifact against `plugins.updater.pubkey`.
 */

import type { AutoUpdateStatus } from '@/common/update/updateTypes';
import { isTauriRuntime } from './tauriRuntime';

// Structural mirror of @tauri-apps/plugin-updater's public surface, so this
// module type-checks without a static import (the plugin loads lazily).
type DownloadEvent =
  | { event: 'Started'; data: { contentLength?: number } }
  | { event: 'Progress'; data: { chunkLength: number } }
  | { event: 'Finished' };

interface TauriUpdate {
  version: string;
  currentVersion: string;
  date?: string;
  body?: string;
  download(onEvent?: (e: DownloadEvent) => void): Promise<void>;
  install(): Promise<void>;
  downloadAndInstall(onEvent?: (e: DownloadEvent) => void): Promise<void>;
  close(): Promise<void>;
}

export interface TauriUpdateInfo {
  version: string;
  /** Version of the currently running bundle (from the Update handle). */
  currentVersion: string;
  releaseNotes?: string;
  releaseDate?: string;
}

// The `Update` is a Rust-side Resource handle: the object returned by `check()`
// must be the one used for `download()`/`install()`. Hold it between calls.
let pendingUpdate: TauriUpdate | null = null;
// Set once `pendingUpdate` has been fully downloaded. A re-check would `close()`
// the handle and discard those bytes (forcing a full re-download), so once true
// we preserve the handle instead of re-checking — see tauriUpdateCheck.
let downloadComplete = false;
// Memoize the in-flight/last check so the modal's autoUpdate.check + update.check
// share ONE round-trip. Checks are also SERIALIZED through it (each chains after
// any in-flight one) so two never run concurrently — concurrent runs would each
// mint an Update handle and leak all but the last, and could clobber the memo.
let checkPromise: Promise<TauriUpdateInfo | null> | null = null;

function infoFromHandle(u: TauriUpdate): TauriUpdateInfo {
  return { version: u.version, currentVersion: u.currentVersion, releaseNotes: u.body, releaseDate: u.date };
}

async function runCheck(): Promise<TauriUpdateInfo | null> {
  const { check } = await import('@tauri-apps/plugin-updater');
  // Free the previous handle before replacing it (releases the Rust resource).
  // Safe to do sequentially because checks are serialized (see tauriUpdateCheck),
  // so no concurrent run is mid-flight on this handle.
  if (pendingUpdate) {
    try {
      await pendingUpdate.close();
    } catch {
      /* handle already gone — ignore */
    }
    pendingUpdate = null;
  }
  const update = (await check()) as TauriUpdate | null;
  pendingUpdate = update;
  return update ? infoFromHandle(update) : null;
}

/**
 * Check for an available update. Resolves to `null` when up to date (or outside
 * the desktop shell). `force` bypasses the memoized result (retry / reopen) but
 * still serializes behind any in-flight check.
 */
export async function tauriUpdateCheck(force = false): Promise<TauriUpdateInfo | null> {
  if (!isTauriRuntime()) return null;
  // Preserve a completed in-session download: re-checking would close the handle
  // and throw away the downloaded bytes, forcing a full re-download.
  if (downloadComplete && pendingUpdate) return infoFromHandle(pendingUpdate);
  if (!force && checkPromise) return checkPromise;
  // Chain after any in-flight check so two never run concurrently.
  const prior = checkPromise;
  const run = (async () => {
    try {
      await prior;
    } catch {
      /* the prior check's failure is its caller's problem — we start fresh */
    }
    return runCheck();
  })();
  checkPromise = run;
  // Identity-guarded clear: only drop the memo if it is still THIS run, so a late
  // failure can't discard a newer in-flight check (the next call then retries).
  run.catch(() => {
    if (checkPromise === run) checkPromise = null;
  });
  return run;
}

// The running bundle version is constant for the session; cache the lookup.
let currentVersionCache: string | null = null;

/**
 * The running app version (`@tauri-apps/api/app` getVersion). Used so the
 * "up to date" screen can show the current version even when `check()` returns
 * `null` (no Update handle in that case). Empty string outside the desktop shell.
 */
export async function tauriUpdateCurrentVersion(): Promise<string> {
  if (!isTauriRuntime()) return '';
  if (currentVersionCache != null) return currentVersionCache;
  const { getVersion } = await import('@tauri-apps/api/app');
  currentVersionCache = await getVersion();
  return currentVersionCache;
}

/**
 * Download the pending update, reporting progress via `emit` (the
 * autoUpdate.status channel the modal subscribes to). Emits `downloading`
 * (with running percent/speed) then a terminal `downloaded`. Re-checks if the
 * handle was lost (e.g. modal reopened after a stale check).
 */
export async function tauriUpdateDownload(emit: (s: AutoUpdateStatus) => void): Promise<void> {
  if (!isTauriRuntime()) throw new Error('Updater is unavailable outside the desktop shell');
  if (!pendingUpdate) await tauriUpdateCheck(true);
  if (!pendingUpdate) throw new Error('No update available to download');
  downloadComplete = false;

  let total = 0;
  let downloaded = 0;
  let speed = 0;
  let lastTs = performance.now();
  let lastBytes = 0;

  emit({ status: 'downloading', progress: { percent: 0, transferred: 0, total: 0, bytesPerSecond: 0 } });

  await pendingUpdate.download((e) => {
    if (e.event === 'Started') {
      total = e.data.contentLength ?? 0;
    } else if (e.event === 'Progress') {
      downloaded += e.data.chunkLength;
      const now = performance.now();
      const dt = now - lastTs;
      // Throttle speed sampling to ~4 Hz; keep the last value between samples so
      // the UI doesn't flicker to 0 on sub-window chunks.
      if (dt >= 250) {
        speed = ((downloaded - lastBytes) / dt) * 1000;
        lastTs = now;
        lastBytes = downloaded;
      }
      emit({
        status: 'downloading',
        progress: {
          percent: total > 0 ? Math.min(100, (downloaded / total) * 100) : 0,
          transferred: downloaded,
          total,
          bytesPerSecond: speed,
        },
      });
    } else if (e.event === 'Finished') {
      const final = total || downloaded;
      emit({
        status: 'downloading',
        progress: { percent: 100, transferred: final, total: final, bytesPerSecond: 0 },
      });
    }
  });

  // Mark the handle as holding downloaded bytes so a subsequent re-check (e.g.
  // the modal being reopened) preserves it instead of discarding the download.
  downloadComplete = true;
  emit({ status: 'downloaded', version: pendingUpdate.version });
}

/**
 * Install the downloaded update (swap the macOS bundle / run the NSIS installer)
 * and relaunch into the new version. No-op outside the desktop shell.
 */
export async function tauriUpdateInstallAndRelaunch(): Promise<void> {
  if (!isTauriRuntime()) return;
  if (pendingUpdate) await pendingUpdate.install();
  const { relaunch } = await import('@tauri-apps/plugin-process');
  await relaunch();
}

// ---------------------------------------------------------------------------
// autoUpdate.status — a renderer-local emitter (the source is the JS download
// callback above, not a Tauri backend event), shaped like tauriShell's
// ShellEmitter so ipcBridge can expose it directly as `autoUpdate.status`.
// ---------------------------------------------------------------------------

function createLocalEmitter<T>() {
  const listeners = new Set<(v: T) => void>();
  return {
    on(cb: (v: T) => void): () => void {
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
    emit(v: T): void {
      listeners.forEach((l) => {
        try {
          l(v);
        } catch {
          /* a listener throwing must not break the others */
        }
      });
    },
  };
}

export const autoUpdateStatusEmitter = createLocalEmitter<AutoUpdateStatus>();
