/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { describe, expect, test } from 'bun:test';
import { CompanionClickThroughController } from './companionClickThroughController';
import type { CompanionLocalPointerSample } from './companionLocalPointer';

const point = (xRatio: number, yRatio: number): CompanionLocalPointerSample => ({
  kind: 'point',
  backend: 'appkit',
  xRatio,
  yRatio,
});

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
};

function makeController(opts: {
  samples?: CompanionLocalPointerSample[];
  sampleError?: Error;
  samplePromise?: Promise<CompanionLocalPointerSample>;
  sample?: () => Promise<CompanionLocalPointerSample>;
  ignored: boolean[];
  hover?: boolean[];
  errors?: unknown[];
  hitTest?: (x: number, y: number) => boolean;
  setIgnore?: (ignore: boolean) => Promise<void>;
  onHoverChange?: (over: boolean) => void;
  onError?: (error: unknown) => void;
}) {
  const queue = [...(opts.samples ?? [])];
  return new CompanionClickThroughController({
    sample: async () => {
      if (opts.sample) return opts.sample();
      if (opts.samplePromise) return opts.samplePromise;
      if (opts.sampleError) throw opts.sampleError;
      const sample = queue.shift();
      if (!sample) throw new Error('test sample queue exhausted');
      return sample;
    },
    setIgnore:
      opts.setIgnore ??
      (async (ignore) => {
        opts.ignored.push(ignore);
      }),
    viewport: () => ({ width: 100, height: 100 }),
    hitTest: opts.hitTest ?? (() => false),
    onHoverChange: opts.onHoverChange ?? ((over) => opts.hover?.push(over)),
    onError: opts.onError ?? ((error) => opts.errors?.push(error)),
  });
}

describe('companion click-through controller', () => {
  test('captures before sampling and flips only after a valid hit decision', async () => {
    const ignored: boolean[] = [];
    const controller = makeController({
      samples: [point(0.5, 0.5), point(1.5, 1.5)],
      ignored,
      hitTest: (x, y) => x === 50 && y === 50,
    });

    await controller.initialize();
    await controller.tick({ captureAll: false, dragging: false });
    await controller.tick({ captureAll: false, dragging: false });

    expect(ignored).toEqual([false, true]);
  });

  test('unsupported Wayland stays captured and uses DOM hover fallback', async () => {
    const ignored: boolean[] = [];
    const hover: boolean[] = [];
    const controller = makeController({
      samples: [{ kind: 'unsupported', backend: 'wayland' }],
      ignored,
      hover,
      hitTest: (x) => x === 20,
    });

    await controller.initialize();
    expect(await controller.tick({ captureAll: false, dragging: false })).toBe('unsupported');
    controller.handleFallbackPointerMove(20, 1);
    controller.handleFallbackPointerLeave();

    expect(ignored).toEqual([false]);
    expect(hover).toEqual([true, false]);
  });

  test('sampling failure captures and enters recovery mode', async () => {
    const ignored: boolean[] = [];
    const controller = makeController({ sampleError: new Error('old shell'), ignored });

    await controller.initialize();

    expect(await controller.tick({ captureAll: false, dragging: false })).toBe('recover');
    expect(ignored).toEqual([false]);
  });

  test('a failed passthrough write is followed by a forced capture write', async () => {
    const ignored: boolean[] = [];
    const errors: unknown[] = [];
    const controller = makeController({
      samples: [point(2, 2)],
      ignored,
      errors,
      setIgnore: async (ignore) => {
        ignored.push(ignore);
        if (ignore) throw new Error('native write outcome is uncertain');
      },
    });

    await controller.initialize();

    expect(await controller.tick({ captureAll: false, dragging: false })).toBe('recover');
    expect(ignored).toEqual([false, true, false]);
    expect(errors).toHaveLength(1);
  });

  test('dispose during an in-flight sample prevents a stale ignore=true write', async () => {
    const pending = deferred<CompanionLocalPointerSample>();
    const ignored: boolean[] = [];
    const controller = makeController({ samplePromise: pending.promise, ignored, hitTest: () => false });

    await controller.initialize();
    const tick = controller.tick({ captureAll: false, dragging: false });
    await controller.dispose();
    pending.resolve(point(2, 2));
    await tick;

    expect(ignored).toEqual([false, false]);
  });

  test('dispose serializes a final capture after an in-flight passthrough write', async () => {
    const passthroughWrite = deferred<void>();
    const passthroughStarted = deferred<void>();
    const ignored: boolean[] = [];
    const controller = makeController({
      samples: [point(2, 2)],
      ignored,
      setIgnore: async (ignore) => {
        ignored.push(ignore);
        if (ignore) {
          passthroughStarted.resolve(undefined);
          await passthroughWrite.promise;
        }
      },
    });

    await controller.initialize();
    const tick = controller.tick({ captureAll: false, dragging: false });
    await passthroughStarted.promise;
    const disposal = controller.dispose();
    passthroughWrite.resolve(undefined);
    await Promise.all([tick, disposal]);

    expect(ignored).toEqual([false, true, false]);
  });

  for (const stateKey of ['dragging', 'captureAll'] as const) {
    test(`rechecks live ${stateKey} after an in-flight sample`, async () => {
      const pending = deferred<CompanionLocalPointerSample>();
      const ignored: boolean[] = [];
      const state = { captureAll: false, dragging: false };
      const controller = makeController({ samplePromise: pending.promise, ignored, hitTest: () => false });

      await controller.initialize();
      const tick = controller.tick(() => state);
      state[stateKey] = true;
      pending.resolve(point(2, 2));
      await tick;

      expect(ignored).toEqual([false]);
    });
  }

  test('a queued live-state guard restores capture even when passthrough was already cached', async () => {
    const ignored: boolean[] = [];
    const state = { captureAll: false, dragging: false };
    let hitCount = 0;
    const controller = makeController({
      samples: [point(2, 2), point(2, 2)],
      ignored,
      hitTest: () => {
        hitCount += 1;
        if (hitCount === 2) state.dragging = true;
        return false;
      },
    });

    await controller.initialize();
    await controller.tick(() => state);
    await controller.tick(() => state);

    expect(ignored).toEqual([false, true, false]);
  });

  test('a throwing hover callback cannot block final capture during disposal', async () => {
    const ignored: boolean[] = [];
    const controller = makeController({
      samples: [point(0.5, 0.5)],
      ignored,
      hitTest: () => true,
      onHoverChange: () => {
        throw new Error('consumer was already unmounted');
      },
    });

    await controller.initialize();
    await controller.tick({ captureAll: false, dragging: false });
    await controller.dispose();

    expect(ignored).toEqual([false, false]);
  });

  test('a throwing error callback cannot block recovery capture', async () => {
    const ignored: boolean[] = [];
    let sampleCount = 0;
    const controller = makeController({
      ignored,
      sample: async () => {
        sampleCount += 1;
        if (sampleCount === 1) return point(2, 2);
        throw new Error('sampling failed');
      },
      onError: () => {
        throw new Error('logger failed');
      },
    });

    await controller.initialize();
    await controller.tick({ captureAll: false, dragging: false });

    expect(await controller.tick({ captureAll: false, dragging: false })).toBe('recover');
    expect(ignored).toEqual([false, true, false]);
  });
});
