/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useEffect, useRef } from 'react';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import { CompanionClickThroughController } from './companionClickThroughController';
import { isPointOverCompanionHitTarget } from './companionHitTarget';
import { getCompanionLocalPointer } from './companionLocalPointer';

const RECOVERY_INTERVAL_MS = 1000;

// React StrictMode or a fast disable/enable can briefly overlap two async effects.
// Only the newest live effect may change the native input policy.
let nextOwnerId = 0;
let activeOwnerId = 0;

export interface CompanionClickThroughOptions {
  /** 仅桌面壳 + 伙伴已启用（窗口可见）时运行。 */
  enabled: boolean;
  /** 命中元素选择器。默认 `[data-companion-hit]`。 */
  hitSelector?: string;
  /** 每个命中包围盒向外扩张的容差（CSS px）。默认 8。 */
  tolerancePx?: number;
  /** 正常采样间隔（ms）。默认 40（约 25fps）。 */
  intervalMs?: number;
  /** 光标进入或离开伙伴交互区时回调，仅在状态改变时触发。 */
  onHoverChange?: (over: boolean) => void;
  /** 临时强制整窗捕获，用于展开输入框、建议弹层等交互面。 */
  captureAll?: boolean;
  /** 拖动期间强制整窗捕获，避免切换原生输入策略造成闪动。 */
  dragging?: boolean;
}

/**
 * 桌面伙伴按区域点击穿透。
 *
 * AppKit、Win32 与 X11 由 Rust 直接采集窗口客户区内的位置，再归一化映射到当前
 * DOM 视口；这条链路不依赖显示器原点或缩放因子。原生 Wayland 无法可靠读取窗口外
 * 指针，因此明确降级为整窗捕获，并用 DOM 事件维持悬停交互。
 */
export function useCompanionClickThrough(opts: CompanionClickThroughOptions): void {
  const {
    enabled,
    hitSelector = '[data-companion-hit]',
    tolerancePx = 8,
    intervalMs = 40,
    onHoverChange,
    captureAll,
    dragging,
  } = opts;

  // 行为选项通过 ref 读取，避免普通状态变化重建原生策略控制器并产生异步交叉写入。
  const hitSelectorRef = useRef(hitSelector);
  hitSelectorRef.current = hitSelector;
  const tolerancePxRef = useRef(tolerancePx);
  tolerancePxRef.current = tolerancePx;
  const intervalMsRef = useRef(intervalMs);
  intervalMsRef.current = intervalMs;
  const onHoverChangeRef = useRef(onHoverChange);
  onHoverChangeRef.current = onHoverChange;
  const captureAllRef = useRef(captureAll);
  captureAllRef.current = captureAll;
  const draggingRef = useRef(dragging);
  draggingRef.current = dragging;

  useEffect(() => {
    if (!enabled || !isTauriRuntime()) return;

    const ownerId = ++nextOwnerId;
    activeOwnerId = ownerId;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let controller: CompanionClickThroughController | null = null;
    let resetNativeCapture: (() => Promise<void>) | null = null;
    let directResetStarted = false;
    let warned = false;

    const warnOnce = (error: unknown): void => {
      if (warned) return;
      warned = true;
      console.warn('[companion] 局部指针采样失败，已回退为整窗捕获；请完整重启桌面壳以加载最新原生命令。', error);
    };

    const requestDirectCapture = (): void => {
      if (directResetStarted) return;
      directResetStarted = true;

      const reset = resetNativeCapture
        ? resetNativeCapture()
        : import('@tauri-apps/api/window').then(async ({ getCurrentWindow }) => {
            if (activeOwnerId === ownerId) await getCurrentWindow().setIgnoreCursorEvents(false);
          });
      void reset.catch(warnOnce);
    };

    const handlePointerMove = (event: PointerEvent): void => {
      controller?.handleFallbackPointerMove(event.clientX, event.clientY);
    };
    const handlePointerLeave = (): void => {
      controller?.handleFallbackPointerLeave();
    };

    const schedule = (delay: number): void => {
      if (disposed) return;
      timer = setTimeout(() => {
        timer = null;
        void runTick();
      }, delay);
    };

    const runTick = async (): Promise<void> => {
      if (disposed || !controller) return;
      const mode = await controller.tick(() => ({
        captureAll: Boolean(captureAllRef.current),
        dragging: Boolean(draggingRef.current),
      }));
      if (!disposed && mode !== 'unsupported') {
        const configuredInterval = intervalMsRef.current;
        const intervalMs = Number.isFinite(configuredInterval) && configuredInterval > 0 ? configuredInterval : 40;
        schedule(mode === 'poll' ? intervalMs : RECOVERY_INTERVAL_MS);
      }
    };

    void (async () => {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      const win = getCurrentWindow();
      resetNativeCapture = async () => {
        if (activeOwnerId === ownerId) await win.setIgnoreCursorEvents(false);
      };

      if (disposed) {
        requestDirectCapture();
        return;
      }

      const nextController = new CompanionClickThroughController({
        sample: getCompanionLocalPointer,
        setIgnore: async (ignore) => {
          if (activeOwnerId === ownerId) await win.setIgnoreCursorEvents(ignore);
        },
        viewport: () => ({ width: window.innerWidth, height: window.innerHeight }),
        hitTest: (clientX, clientY) =>
          isPointOverCompanionHitTarget(
            clientX,
            clientY,
            document.querySelectorAll<HTMLElement>(hitSelectorRef.current),
            { tolerancePx: tolerancePxRef.current }
          ),
        onHoverChange: (over) => onHoverChangeRef.current?.(over),
        onError: warnOnce,
      });
      controller = nextController;

      window.addEventListener('pointermove', handlePointerMove);
      window.addEventListener('pointerleave', handlePointerLeave);
      await nextController.initialize();
      if (disposed) {
        await nextController.dispose();
        return;
      }
      schedule(0);
    })().catch(async (error) => {
      warnOnce(error);
      if (controller) await controller.dispose();
      else requestDirectCapture();
    });

    return () => {
      disposed = true;
      if (timer) clearTimeout(timer);
      window.removeEventListener('pointermove', handlePointerMove);
      window.removeEventListener('pointerleave', handlePointerLeave);

      if (controller) void controller.dispose();
      else requestDirectCapture();
    };
  }, [enabled]);
}

export default useCompanionClickThrough;
