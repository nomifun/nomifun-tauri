/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import { useEffect, useRef } from 'react';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import { isPointOverCompanionHitTarget } from './companionHitTarget';

/**
 * 桌面伙伴「按区域点击穿透」。
 *
 * 桌宠窗口是透明、无边框、置顶的 Tauri 窗口（桌面态 240×320，聊天态被 enterChatSize
 * 放大到最大 ~560×720）。WebView2/wry 在 Windows 上让**整个窗口 HWND** 捕获鼠标——
 * 透明像素并不会把点击放行给底层应用。于是伙伴四周的大片透明区域成了「超大遮罩」，
 * 挡住底层软件的点击。
 *
 * 修法：默认对整窗 `setIgnoreCursorEvents(true)`（全穿透），只在光标落在**伙伴实体或
 * 当前可交互 UI**（标了 `data-companion-hit` 的元素：立绘 / 气泡 / 输入条 / 角标 /
 * 建议弹层）的包围盒内时才切回 `false`（捕获，可点可拖可悬停）。右键菜单使用 OS
 * 原生 popup menu，不受 webview 边界裁切，也不参与命中区域判定。
 *
 * 为什么靠 OS 全局光标轮询而非 DOM 事件：一旦 ignore=true，webview 就再也收不到任何
 * mousemove，无法靠 DOM 事件察觉光标重新移回立绘（鸡生蛋）。`cursorPosition()` 直接
 * 读 OS 光标（屏幕物理坐标，原点=桌面左上），与 `outerPosition()` 同坐标系，换算成窗口
 * 本地逻辑坐标后与 `getBoundingClientRect()` 比较即可，与 ignore 状态无关。
 *
 * 性能：`setIgnoreCursorEvents` 只在「命中态翻转」时调用（边沿触发，绝非逐帧），
 * 翻转才碰 WS_EX_TRANSPARENT，无闪烁；每个 tick 只有一次 `cursorPosition()` IPC。
 * 失败降级：任何一步抛错都把 ignore 复位为 false（=未修前的行为，伙伴始终可点），
 * 绝不让伙伴卡在「全穿透→点不到」。
 */
export interface CompanionClickThroughOptions {
  /** 仅桌面壳 + 伙伴已启用（窗口可见）时运行；否则不轮询、并把 ignore 复位为 false。 */
  enabled: boolean;
  /** 命中元素选择器。默认 `[data-companion-hit]`。 */
  hitSelector?: string;
  /** 每个命中包围盒向外扩张的容差（CSS px）：桥接立绘↔气泡/输入条之间的 margin 缝隙，
   *  并给「抓得住」一点余量。默认 8。 */
  tolerancePx?: number;
  /** 轮询间隔（ms）。默认 40（~25fps）：悬停/点击响应够灵敏，又不至于太费。 */
  intervalMs?: number;
  /** 光标「是否落在交互区」翻转时回调（用于让悬停才出现的输入条同步显隐 + 进出命中集）。
   *  仅在状态改变时触发，不会逐帧调用。 */
  onHoverChange?: (over: boolean) => void;
  /** 瞬态「整窗捕获」开关：为 true 时跳过按区域穿透、强制整窗捕获鼠标（ignore=false）。
   *  用于建议弹层、展开输入框等 webview 内瞬态交互面。通过 ref 读取，改动不重启轮询循环。 */
  captureAll?: boolean;
  /** 拖动伙伴期间为 true：冻结按区域判定，保持整窗捕获(ignore=false)、不翻转、不回调，
   *  根除拖动中 setIgnoreCursorEvents 反复翻转引起的透明窗 DWM 重合成闪动。
   *  通过 ref 读取，改动不重启轮询循环。 */
  dragging?: boolean;
}

export function useCompanionClickThrough(opts: CompanionClickThroughOptions): void {
  const { enabled, hitSelector = '[data-companion-hit]', tolerancePx = 8, intervalMs = 40, onHoverChange, captureAll, dragging } = opts;

  // 通过 ref 读 captureAll / dragging，使其变化不触发 effect 重启 / 监听器抖动。
  const captureAllRef = useRef(captureAll);
  captureAllRef.current = captureAll;
  const draggingRef = useRef(dragging);
  draggingRef.current = dragging;

  useEffect(() => {
    if (!enabled || !isTauriRuntime()) return;

    let disposed = false;
    let timer: ReturnType<typeof setInterval> | null = null;
    let heartbeat: ReturnType<typeof setInterval> | null = null;
    let unlistenMoved: (() => void) | undefined;
    let unlistenResized: (() => void) | undefined;
    // 缓存窗口外缘位置（屏幕物理坐标）。companion 窗 decorations(false)+shadow(false)，
    // outerPosition === 客户区左上，故无需扣标题栏。位置变更经 onMoved/onResized 刷新，
    // 另加 1s 心跳兜底，防止漏掉某次事件造成的坐标漂移。
    let originX = 0;
    let originY = 0;
    let originReady = false;
    // 上一次写入的 ignore 状态（null=尚未写入）。仅在翻转时调用原生切换。
    let lastIgnore: boolean | null = null;
    // 上一次上报的「命中态」（null=尚未上报）。仅在翻转时回调宿主。
    let lastOver: boolean | null = null;
    // 仅警告一次：穿透权限缺失（多发于 desktop:dev 只热更前端、Rust 壳未重编）。
    let warnedPermission = false;
    let ticking = false;

    void (async () => {
      const winApi = await import('@tauri-apps/api/window');
      if (disposed) return;
      const win = winApi.getCurrentWindow();
      const { cursorPosition } = winApi;

      const refreshOrigin = async (): Promise<void> => {
        try {
          const pos = await win.outerPosition();
          originX = pos.x;
          originY = pos.y;
          originReady = true;
        } catch {
          // 取不到就保持旧值；下一次事件/心跳再试。
        }
      };

      const setIgnore = async (ignore: boolean): Promise<void> => {
        if (ignore === lastIgnore) return;
        lastIgnore = ignore;
        try {
          await win.setIgnoreCursorEvents(ignore);
        } catch {
          // 切换失败：放弃记忆该状态，下个 tick 重试。
          lastIgnore = null;
        }
      };

      // 命中判定：光标本地逻辑坐标是否落在任一可接收 pointer 的 [data-companion-hit]
      // 元素包围盒（含容差）内；alpha 掩码（自定义立绘）由 companionHitTarget 继续精判。
      const isOverHit = (clientX: number, clientY: number): boolean => {
        return isPointOverCompanionHitTarget(clientX, clientY, document.querySelectorAll<HTMLElement>(hitSelector), {
          tolerancePx,
        });
      };

      let wasDragging = false;
      const tick = async (): Promise<void> => {
        if (disposed || ticking || !originReady) return;
        ticking = true;
        try {
          // 拖动期间：保持整窗捕获，不重算 over、不翻转 ignore、不回调 onHoverChange。
          // 立绘上 lastIgnore 已是 false，setIgnore(false) 边沿触发为 no-op → 拖动全程
          // setIgnoreCursorEvents 原生调用 0 次 → 无 DWM 重合成闪动。
          if (draggingRef.current) {
            wasDragging = true;
            await setIgnore(false);
            return;
          }
          // 拖动刚结束：先刷新最终窗口位置（拖动中 origin 滞后未刷），再恢复按区域判定。
          if (wasDragging) {
            wasDragging = false;
            await refreshOrigin();
          }
          // webview 内瞬态交互面打开：整窗捕获，避免点空白区泄漏到底层。
          // 不动 lastOver/onHoverChange——浮层关闭后下个 tick 恢复按区域判定。
          if (captureAllRef.current) {
            await setIgnore(false);
            return;
          }
          const cur = await cursorPosition(); // 屏幕物理坐标
          const dpr = window.devicePixelRatio || 1;
          const clientX = (cur.x - originX) / dpr;
          const clientY = (cur.y - originY) / dpr;
          const over = isOverHit(clientX, clientY);
          if (over !== lastOver) {
            lastOver = over;
            onHoverChange?.(over);
          }
          await setIgnore(!over);
        } catch (e) {
          // 读光标/换算失败：复位为捕获（=未修前行为，伙伴可点），避免卡在全穿透。
          // 多半是 cursor-position / set-ignore-cursor-events 权限未生效——desktop:dev
          // 只热更了前端、Rust 壳没重编时常见。提示一次，便于定位（需完整重启桌面应用）。
          if (!warnedPermission) {
            warnedPermission = true;
            console.warn(
              '[companion] 点击穿透未生效：window cursor-position / set-ignore-cursor-events 权限调用失败。' +
                '若用 desktop:dev，请完整重启（重编 Rust 壳）以嵌入新 capabilities。',
              e
            );
          }
          await setIgnore(false);
        } finally {
          ticking = false;
        }
      };

      await refreshOrigin();
      if (disposed) return;
      unlistenMoved = await win.onMoved(() => void refreshOrigin());
      unlistenResized = await win.onResized(() => void refreshOrigin());
      if (disposed) {
        unlistenMoved?.();
        unlistenResized?.();
        return;
      }
      timer = setInterval(() => void tick(), intervalMs);
      heartbeat = setInterval(() => void refreshOrigin(), 1000);
      void tick();
    })();

    return () => {
      disposed = true;
      if (timer) clearInterval(timer);
      if (heartbeat) clearInterval(heartbeat);
      unlistenMoved?.();
      unlistenResized?.();
      // 停止轮询时把命中态归零，避免「悬停才出现」的输入条卡在显示态。
      if (lastOver === true) onHoverChange?.(false);
      // 退出时复位为捕获：窗口若只是隐藏（伙伴停用）后再显示，不会卡在全穿透。
      if (lastIgnore === true) {
        void import('@tauri-apps/api/window')
          .then(({ getCurrentWindow }) => getCurrentWindow().setIgnoreCursorEvents(false))
          .catch(() => {});
      }
    };
  }, [enabled, hitSelector, tolerancePx, intervalMs, onHoverChange]);
}

export default useCompanionClickThrough;
