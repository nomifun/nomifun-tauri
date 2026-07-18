/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { toCompanionClientPoint, type CompanionLocalPointerSample } from './companionLocalPointer';

export type CompanionClickThroughMode = 'poll' | 'recover' | 'unsupported';

export interface CompanionClickThroughControllerDeps {
  sample: () => Promise<CompanionLocalPointerSample>;
  setIgnore: (ignore: boolean) => Promise<void>;
  viewport: () => { width: number; height: number };
  hitTest: (clientX: number, clientY: number) => boolean;
  onHoverChange?: (over: boolean) => void;
  onError?: (error: unknown) => void;
}

export interface CompanionClickThroughState {
  captureAll: boolean;
  dragging: boolean;
}

export type CompanionClickThroughStateSource = CompanionClickThroughState | (() => CompanionClickThroughState);

/**
 * Owns the safety-critical native input-region transitions independently of React.
 * Every uncertain state converges to whole-window capture (`ignore=false`).
 */
export class CompanionClickThroughController {
  private disposed = false;
  private lastIgnore: boolean | null = null;
  private lastOver: boolean | null = null;
  private ignoreQueue: Promise<void> = Promise.resolve();
  private disposePromise: Promise<void> | null = null;

  mode: CompanionClickThroughMode = 'poll';

  constructor(private readonly deps: CompanionClickThroughControllerDeps) {}

  async initialize(): Promise<void> {
    if (this.disposed) return;
    try {
      // Never inherit a stale passthrough state from a hidden/reused native window.
      await this.queueIgnore(false, { force: true });
      if (!this.disposed) this.mode = 'poll';
    } catch (error) {
      if (!this.disposed) {
        this.mode = 'recover';
        this.notifyError(error);
      }
    }
  }

  async tick(stateSource: CompanionClickThroughStateSource): Promise<CompanionClickThroughMode> {
    if (this.disposed) return this.mode;

    const readState =
      typeof stateSource === 'function' ? stateSource : (): CompanionClickThroughState => stateSource;

    if (this.requiresCapture(readState())) {
      try {
        await this.applyIgnore(false);
      } catch (error) {
        await this.enterRecovery(error, false);
      }
      return this.mode;
    }

    // If initialization or a previous capture write failed, prove that capture works
    // before allowing this tick to consider passthrough again.
    if (this.mode === 'recover' && this.lastIgnore !== false) {
      try {
        await this.applyIgnore(false);
      } catch (error) {
        await this.enterRecovery(error, false);
        return this.mode;
      }
    }

    let requestedIgnore: boolean | null = null;
    try {
      const sample = await this.deps.sample();
      if (this.disposed) return this.mode;

      if (sample.kind === 'unsupported') {
        requestedIgnore = false;
        await this.applyIgnore(false);
        if (!this.disposed) this.mode = 'unsupported';
        return this.mode;
      }

      // The sample IPC can overlap a React state transition. Re-read the live
      // refs before any hit decision so an old tick cannot interrupt a drag or
      // a newly opened whole-window interaction surface.
      if (this.requiresCapture(readState())) {
        requestedIgnore = false;
        await this.applyIgnore(false);
        return this.mode;
      }

      const client = toCompanionClientPoint(sample, this.deps.viewport());
      if (!client) throw new Error('invalid companion pointer viewport');
      const over = this.deps.hitTest(client.x, client.y);
      this.reportHover(over);

      requestedIgnore = !over;
      await this.applyIgnore(
        requestedIgnore,
        requestedIgnore ? () => !this.requiresCapture(readState()) : undefined
      );
      if (!this.disposed) this.mode = 'poll';
    } catch (error) {
      await this.enterRecovery(error, requestedIgnore === true);
    }

    return this.mode;
  }

  handleFallbackPointerMove(clientX: number, clientY: number): void {
    if (this.disposed || this.mode === 'poll') return;
    try {
      this.reportHover(this.deps.hitTest(clientX, clientY));
    } catch (error) {
      this.notifyError(error);
      this.clearHover();
    }
  }

  handleFallbackPointerLeave(): void {
    if (!this.disposed && this.mode !== 'poll') this.clearHover();
  }

  dispose(): Promise<void> {
    if (this.disposePromise) return this.disposePromise;

    this.disposed = true;
    // The queue makes this write run after any already-started passthrough write,
    // so a late native response can never leave a hidden/reused window unclickable.
    this.disposePromise = this.queueIgnore(false, { force: true, allowDisposed: true }).catch((error) => {
      this.notifyError(error);
    });
    this.clearHover();
    return this.disposePromise;
  }

  private applyIgnore(ignore: boolean, guard?: () => boolean): Promise<void> {
    return this.queueIgnore(ignore, { guard });
  }

  private queueIgnore(
    ignore: boolean,
    options: { force?: boolean; allowDisposed?: boolean; guard?: () => boolean } = {}
  ): Promise<void> {
    if (this.disposed && !options.allowDisposed) return Promise.resolve();

    const operation = this.ignoreQueue.then(async () => {
      if (this.disposed && !options.allowDisposed) return;
      if (options.guard && !options.guard()) {
        // The interaction state changed while this operation waited in the
        // microtask/native-write queue. A cached passthrough state must be
        // actively reversed rather than treated as an ordinary duplicate.
        if (ignore && this.lastIgnore !== false) {
          await this.deps.setIgnore(false);
          this.lastIgnore = false;
        }
        return;
      }
      if (!options.force && this.lastIgnore === ignore) return;

      await this.deps.setIgnore(ignore);
      this.lastIgnore = ignore;
    });

    // Keep the serialization chain usable after a rejected native write while
    // still returning the original rejection to the caller that must recover.
    this.ignoreQueue = operation.catch(() => {});
    return operation;
  }

  private async enterRecovery(error: unknown, forceCapture: boolean): Promise<void> {
    if (this.disposed) return;

    this.mode = 'recover';
    this.notifyError(error);
    try {
      await this.queueIgnore(false, { force: forceCapture });
    } catch (captureError) {
      this.notifyError(captureError);
    }
  }

  private reportHover(over: boolean): void {
    if (this.lastOver === over) return;
    this.lastOver = over;
    try {
      this.deps.onHoverChange?.(over);
    } catch (error) {
      this.notifyError(error);
    }
  }

  private clearHover(): void {
    if (this.lastOver === true) this.reportHover(false);
  }

  private requiresCapture(state: CompanionClickThroughState): boolean {
    return state.captureAll || state.dragging;
  }

  private notifyError(error: unknown): void {
    try {
      this.deps.onError?.(error);
    } catch {
      // Diagnostic callbacks must never interfere with native input recovery.
    }
  }
}
