/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ipcBridge } from '@/common';
import { isBackendHttpError } from '@/common/adapter/httpBridge';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import type { ICompanionProfile, ICompanionSuggestion, IResponseMessage } from '@/common/adapter/ipcBridge';
import { extractResponseTextChunk } from '@/common/chat/displayText';
import MarkdownView from '@/renderer/components/Markdown';
import LocalImageView from '@/renderer/components/media/LocalImageView';
import { usePasteService } from '@/renderer/hooks/file/usePasteService';
import { imageExts, type FileMetadata } from '@/renderer/services/FileService';
import { useUploadState } from '@/renderer/hooks/file/useUploadState';
import { buildDisplayMessage } from '@/renderer/utils/file/messageFiles';
import { THEME_SYNC_EVENT, type ThemeSyncPayload } from '@/renderer/utils/theme/themeBroadcast';
import { companionErrorKey, streamErrorCode } from './companionError';
import { isForCompanion } from './eventScope';
import { injectCompanionCustomCss } from '@/renderer/utils/theme/applyCustomCss';
import { configService } from '@/common/config/configService';
import ChannelDingTalkLogo from '@/renderer/assets/channel-logos/dingtalk.svg';
import ChannelDiscordLogo from '@/renderer/assets/channel-logos/discord.svg';
import ChannelLarkLogo from '@/renderer/assets/channel-logos/lark.svg';
import ChannelSlackLogo from '@/renderer/assets/channel-logos/slack.svg';
import ChannelTelegramLogo from '@/renderer/assets/channel-logos/telegram.svg';
import ChannelWecomLogo from '@/renderer/assets/channel-logos/wecom.svg';
import ChannelWeixinLogo from '@/renderer/assets/channel-logos/weixin.svg';
import CompanionAvatar from './CompanionAvatar';
import { browserNarrationFor } from './browserNarration';
import { getDeskSpecFor } from './characters';
import { customFigureMetaOf } from './characters/customMeta';
import type { CompanionActivity as RabbitActivity, CompanionMood as RabbitMood } from './characters';
import { placeResizedWindow } from './windowGeometry';
import { buildCompanionMenuEntries, type CompanionMenuAction } from './companionNativeMenu';
import { useCompanionClickThrough } from './useCompanionClickThrough';
import { createCompanionBarRevealController, type CompanionBarRevealController } from './companionBarReveal';
import { shouldCaptureWholeCompanionWindow } from './companionCapturePolicy';
import './companion.css';

const BUBBLE_MS = 12_000;
/** Safety net: a streaming bubble auto-dismisses even if chat-done is lost. */
const STREAM_STALL_MS = 45_000;
const INIT_RETRY_MS = 5_000;
const INIT_MAX_RETRIES = 6;
const BAR_REVEAL_HIDE_DELAY_MS = 280;

/** Platform → bubble-header logo for remote IM turns (keys follow the
 *  backend's PluginType::Display strings on `channel_platform`). */
const CHANNEL_LOGOS: Record<string, string> = {
  telegram: ChannelTelegramLogo,
  lark: ChannelLarkLogo,
  dingtalk: ChannelDingTalkLogo,
  weixin: ChannelWeixinLogo,
  wecom: ChannelWecomLogo,
  slack: ChannelSlackLogo,
  discord: ChannelDiscordLogo,
};

/** "HH:mm-HH:mm" quiet window check (supports overnight ranges). */
const inQuietHours = (start: string, end: string): boolean => {
  if (!start || !end) return false;
  const parse = (s: string): number | null => {
    const m = /^(\d{1,2}):(\d{2})$/.exec(s.trim());
    if (!m) return null;
    return Number(m[1]) * 60 + Number(m[2]);
  };
  const s = parse(start);
  const e = parse(end);
  if (s == null || e == null) return false;
  const now = new Date();
  const cur = now.getHours() * 60 + now.getMinutes();
  return s <= e ? cur >= s && cur < e : cur >= s || cur < e;
};

/** Owning companion id from the window URL (`index.html#/companion?companionId={companion_id}`). */
const parseCompanionIdFromHash = (): string | null => {
  if (typeof window === 'undefined') return null;
  const hash = window.location.hash; // "#/companion?companionId={companion_id}"
  const q = hash.indexOf('?');
  if (q === -1) return null;
  const id = new URLSearchParams(hash.slice(q + 1)).get('companionId');
  return id && id.trim() ? id.trim() : null;
};

/**
 * The desktop-companion window page (route #/companion?companionId={companion_id}, window label
 * "companion-{companion_id}"). Renders that companion's character on a transparent always-on-top
 * window; shows/hides the native window from the companion's persisted profile
 * (appearance.companion_enabled); hover reveals the chat bar and replies stream into
 * the forehead bubble. Without a companionId query (direct open / web preview) it
 * falls back to the first enabled companion in the registry.
 */
const CompanionPage: React.FC = () => {
  const { t } = useTranslation();
  const [companionId, setCompanionId] = useState<string | null>(parseCompanionIdFromHash);
  const [profile, setProfile] = useState<ICompanionProfile | null>(null);
  const [mood, setMood] = useState<RabbitMood>('content');
  const [activity, setActivity] = useState<RabbitActivity>('idle');
  const [bubble, setBubble] = useState<string>('');
  const [bubbleLoading, setBubbleLoading] = useState(false);
  const [unread, setUnread] = useState(0);
  const [suggestions, setSuggestions] = useState<ICompanionSuggestion[]>([]);
  const [showSuggestions, setShowSuggestions] = useState(false);
  const [input, setInput] = useState('');
  const [sending, setSending] = useState(false);
  /** 光标是否停在伙伴交互区（由 useCompanionClickThrough 上报）：驱动「悬停才出现」的
   *  迷你输入条显隐 + 进出点击穿透命中集。替代纯 CSS :hover（穿透态下 webview 收不到
   *  hover 事件，:hover 不可靠）。 */
  const [barRevealed, setBarRevealed] = useState(false);
  const barRevealControllerRef = useRef<CompanionBarRevealController | null>(null);
  /** 展开态快捷输入（多行文字 + 粘贴/附加图片）。迷你态为单行 input。 */
  const [composerOpen, setComposerOpen] = useState(false);
  const [composerText, setComposerText] = useState('');
  /** 已附加的图片文件绝对路径（粘贴或附件选择得到）。 */
  const [attachedFiles, setAttachedFiles] = useState<string[]>([]);
  /** 展开时预解析的会话 id（供粘贴上传关联；best-effort）。 */
  const [composerThreadId, setComposerThreadId] = useState<number | null>(null);
  /** 拖拽图片到桌宠窗时的高亮态（由 Tauri 原生 onDragDropEvent 驱动）。 */
  const [dragOver, setDragOver] = useState(false);
  /** 正在拖动伙伴 / 刚拖完：冻结点击穿透轮询以根除拖动闪动。 */
  const [dragging, setDragging] = useState(false);
  /** 立绘命中元素 ref：传给 CompanionAvatar→CustomFigure 挂 alpha 掩码。 */
  const figureHitRef = useRef<HTMLDivElement | null>(null);
  /** 'sendbox' 上传进度：粘贴/选择图片落盘期间用于 loading 态与暂禁发送。 */
  const sendboxUpload = useUploadState('sendbox');
  /** 本地回合是否正在流式生成（驱动气泡上的「打断 □」按钮显隐）。镜像 turnActiveRef。 */
  const [bubbleRunning, setBubbleRunning] = useState(false);
  const bubbleTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const profileRef = useRef<ICompanionProfile | null>(null);
  profileRef.current = profile;
  const suggestionsRef = useRef<ICompanionSuggestion[]>([]);
  suggestionsRef.current = suggestions;
  /** Set while the user is dragging / just dragged, so a config-updated echo
   *  doesn't snap the window back to a stale position. */
  const lastLocalMoveAt = useRef(0);
  /** Active companion thread (a real nomi conversation id). */
  const activeThreadRef = useRef<number | null>(null);
  /** True from send until finish/error/stall. NO LONGER gates rendering
   *  (that gate dropped badcase-3 replies); it now only drives the stall vs.
   *  dismiss timer choice in the stream handler. */
  const turnActiveRef = useRef(false);
  /** Per-segment text buffers for the in-flight turn (a nomi turn can span
   *  several assistant msg_ids; rendered joined in arrival order). */
  const segmentsRef = useRef(new Map<string, string>());
  const segmentOrderRef = useRef<string[]>([]);
  /** True while the native window has been grown to chat size (elastic bubble
   *  needs room beyond the 240×320 desk window). Guards against thrashing
   *  setSize per streaming chunk — we resize once entering chat mode and once
   *  restoring the desk size when the bubble clears. */
  const chatSizedRef = useRef(false);
  /** 气泡正文滚动视口 + 内容包裹，用于「粘底自动追尾」（持续对话感）。 */
  const bubbleScrollRef = useRef<HTMLDivElement | null>(null);
  const bubbleContentRef = useRef<HTMLDivElement | null>(null);
  /** true = 用户停在底部，新流式文本到达就钉底；用户上滑读历史则置 false 不抢滚。 */
  const bubbleStickRef = useRef(true);
  /** 上一帧气泡文本长度：变短=换轮/中间件 replace 终稿覆盖 → 重新钉底。 */
  const bubblePrevLenRef = useRef(0);
  /** 用户「× 忽略」后置真：压制本轮后续所有流式渲染（防 stop 异步期间残片回闪重现气泡）。
   *  每次新的本地发送（submitTurn）重置为 false。 */
  const bubbleDismissedRef = useRef(false);
  /** Remote IM turn bubble header (platform + the visitor's message);
   *  null = the bubble shows local content. */
  const [remoteHeader, setRemoteHeader] = useState<{ platform: string; inbound: string } | null>(null);
  /** Single-slot buffer for the in-flight remote IM turn (channel master
   *  conversations bound to this companion; a newer turn replaces the slot). Kept
   *  separate from the local turn's segments so the two never clobber. */
  const remoteTurnRef = useRef<{
    conversationId: number;
    platform: string;
    inbound: string;
    segments: Map<string, string>;
    order: string[];
    /** Set on finish/error; a later fragment with a new msg_id then means a
     *  new turn in the same IM chat (its userCreated was missed) — reset. */
    finished: boolean;
  } | null>(null);

  /** 远程 IM 自动回复回合是否正在生成 —— 让「打断 □」也能对远程会话生效（点 6）。
   *  state 驱动按钮显隐；ref 供事件闭包内的 interrupt/dismiss 即时读取。 */
  const [remoteRunning, setRemoteRunning] = useState(false);
  const remoteRunningRef = useRef(false);
  const markRemoteRunning = useCallback((v: boolean) => {
    if (remoteRunningRef.current === v) return;
    remoteRunningRef.current = v;
    setRemoteRunning(v);
  }, []);

  useEffect(() => {
    const controller = createCompanionBarRevealController({
      hideDelayMs: BAR_REVEAL_HIDE_DELAY_MS,
      setRevealed: setBarRevealed,
    });
    barRevealControllerRef.current = controller;
    return () => {
      controller.dispose();
      if (barRevealControllerRef.current === controller) {
        barRevealControllerRef.current = null;
      }
    };
  }, []);

  const handleCompanionHoverChange = useCallback((over: boolean) => {
    barRevealControllerRef.current?.handleHoverChange(over);
  }, []);

  /** 鼠标悬停气泡时暂停「消散」，移开后重新计时——长回复不会没读完就消失（点 1）。 */
  const bubbleHoveredRef = useRef(false);
  const clearBubbleTimer = useCallback(() => {
    if (bubbleTimer.current) {
      clearTimeout(bubbleTimer.current);
      bubbleTimer.current = null;
    }
  }, []);
  /** 安排一次气泡消散：到点时若鼠标仍停在气泡上，则推迟（短间隔重试）而非真正消散，
   *  直到鼠标移开。所有消散路径（收尾/出错/打断/远程/stall）统一走这里，于是 hover
   *  暂停对它们天然全部生效，无需在每个站点重复判断。 */
  const armBubbleDismiss = useCallback((ms: number, action: () => void) => {
    if (bubbleTimer.current) clearTimeout(bubbleTimer.current);
    const tick = (delay: number) => {
      bubbleTimer.current = setTimeout(() => {
        if (bubbleHoveredRef.current) {
          tick(700);
          return;
        }
        action();
      }, delay);
    };
    tick(ms);
  }, []);

  const popBubble = useCallback(
    (text: string) => {
      const cfg = profileRef.current;
      if (cfg && inQuietHours(cfg.appearance.quiet_start, cfg.appearance.quiet_end)) return;
      setRemoteHeader(null);
      setBubble(text);
      armBubbleDismiss(BUBBLE_MS, () => setBubble(''));
    },
    [armBubbleDismiss]
  );

  // Apply native window visibility/position from the companion's profile (desktop shell only).
  const applyWindowState = useCallback(async (cfg: ICompanionProfile, opts?: { skipPosition?: boolean }) => {
    if (!isTauriRuntime()) return;
    try {
      const { getCurrentWindow, PhysicalPosition, availableMonitors, primaryMonitor } = await import('@tauri-apps/api/window');
      const win = getCurrentWindow();
      if (!cfg.appearance.companion_enabled) {
        await win.hide();
        return;
      }
      // Capture visibility BEFORE any show(). On macOS `win.show()` maps to tao
      // set_visible(true) → makeKeyAndOrderFront, which steals key focus from the
      // main window — and re-keys even an already-visible window. This handler
      // re-runs on every config-updated echo (incl. our own debounced drag-save
      // round-trip), so an unconditional show() grabbed focus on every echo. Only
      // show() when the window is actually hidden (a real enable / first init);
      // an already-visible companion keeps its current focus state.
      const wasVisible = await win.isVisible();
      let target: { x: number; y: number } | null = null;
      if (!opts?.skipPosition && cfg.appearance.companion_x != null && cfg.appearance.companion_y != null) {
        target = { x: cfg.appearance.companion_x, y: cfg.appearance.companion_y };
        // A saved position can point at no connected monitor: configs written
        // by older builds (launch-time scale bug doubled the coords every
        // start on Retina) or a since-unplugged external display. Restoring
        // it verbatim parks the companion off-screen forever, so fall back to the
        // primary monitor's bottom-right; the onMoved echo then persists the
        // healed coords. Validation is best-effort — on failure keep the raw
        // saved position rather than blocking show().
        try {
          const [monitors, size] = await Promise.all([availableMonitors(), win.outerSize()]);
          if (monitors.length > 0) {
            const MIN_REACHABLE = 24; // px of the window that must stay grabbable
            const onScreen = monitors.some((m) => {
              const overlapX = Math.min(target!.x + size.width, m.position.x + m.size.width) - Math.max(target!.x, m.position.x);
              const overlapY = Math.min(target!.y + size.height, m.position.y + m.size.height) - Math.max(target!.y, m.position.y);
              return overlapX >= MIN_REACHABLE && overlapY >= MIN_REACHABLE;
            });
            if (!onScreen) {
              const p = (await primaryMonitor()) ?? monitors[0];
              target = {
                x: p.position.x + p.size.width - size.width - 24,
                y: p.position.y + p.size.height - size.height - 96,
              };
            }
          }
        } catch {
          // keep raw saved position
        }
        await win.setPosition(new PhysicalPosition(target.x, target.y));
      }
      if (!wasVisible) {
        await win.show();
      }
      // Re-apply after show(): before its first orderFront a window is not
      // attached to a screen, and on macOS Retina the physical→logical
      // conversion then runs with scale 1.0 — the window lands at 2× the
      // intended position and the onMoved echo persists the doubled coords
      // (compounding every launch until the companion is off-screen). Once visible
      // the scale is correct; this second call is idempotent elsewhere.
      if (target) {
        await win.setPosition(new PhysicalPosition(target.x, target.y));
      }
    } catch (e) {
      console.error('companion window state apply failed:', e);
    }
  }, []);

  // Match the native window to the character's desk spec (full-figure
  // characters use a taller window; the other five keep the classic 240x320).
  // Bottom-anchored and monitor-clamped — and only at actual size changes, so
  // a user's deliberate half-off-screen placement is never disturbed by
  // ordinary restores. Must run AFTER applyWindowState's show(): before the
  // first orderFront macOS reports scaleFactor 1.0 and every physical-px
  // computation here would be wrong.
  const applyDeskSize = useCallback(async (cfg: ICompanionProfile, opts?: { anchor?: 'bottom' | 'top-left' }) => {
    if (!isTauriRuntime() || !cfg.appearance.companion_enabled) return;
    // 聊天模式下窗口被 enterChatSize 故意放大；此时 config-updated 回声里的
    // applyDeskSize 不能把它缩回桌面伙伴尺寸（会和 enterChatSize 打架、把气泡挤没）。
    // exitChatSize 会先清掉该标记再调本函数还原，故不影响正常收尾还原。
    if (chatSizedRef.current) return;
    try {
      const { getCurrentWindow, PhysicalPosition, PhysicalSize, availableMonitors } = await import('@tauri-apps/api/window');
      const win = getCurrentWindow();
      const desk = getDeskSpecFor(cfg.character, customFigureMetaOf(cfg));
      const [pos, size, scale] = await Promise.all([win.outerPosition(), win.outerSize(), win.scaleFactor()]);
      const target = { width: Math.round(desk.windowWidth * scale), height: Math.round(desk.windowHeight * scale) };
      // outerSize === innerSize for companion windows (decorations(false) + shadow(false)).
      if (size.width === target.width && size.height === target.height) return;
      let monitors: { x: number; y: number; width: number; height: number }[] = [];
      try {
        monitors = (await availableMonitors()).map((m) => ({
          x: m.position.x,
          y: m.position.y,
          width: m.size.width,
          height: m.size.height,
        }));
      } catch {
        // place unclamped
      }
      await win.setSize(new PhysicalSize(target.width, target.height));
      // Read back what the OS actually granted (a resizable(false) window can
      // refuse programmatic resizes on some window managers) and anchor from
      // the achieved size — never reposition for a resize that didn't happen.
      // Note: on fully-async WMs (X11) this read-back can race the grant; the
      // failure mode is a one-time skipped anchor, self-healed on next apply.
      const achieved = await win.outerSize();
      if (achieved.width === size.width && achieved.height === size.height) return;
      // 'bottom' (default): live character switch — the window and the saved
      // coords are the same generation, so anchor the bottom edge and grow up.
      // 'top-left': cold-start restore — saved coords are the TALL window's
      // top-left, but the freshly created window is still 240x320; bottom-
      // anchoring from that small rect would climb 280px every launch and
      // compound through the onMoved persistence. Keep the top-left, clamp only.
      const anchorRect =
        opts?.anchor === 'top-left'
          ? { x: pos.x, y: pos.y, width: achieved.width, height: achieved.height }
          : { x: pos.x, y: pos.y, width: size.width, height: size.height };
      const next = placeResizedWindow(anchorRect, achieved, monitors);
      await win.setPosition(new PhysicalPosition(next.x, next.y));
      // The move is ours: keep the config-updated echo from snapping us back,
      // and let the onMoved handler persist the new coords.
      lastLocalMoveAt.current = Date.now();
    } catch (e) {
      console.error('companion desk size apply failed:', e);
    }
  }, []);

  // ---- 聊天模式窗口尺寸（弹性气泡，badcase 3）----
  // 桌面伙伴原生窗口只有 240×320，父容器 overflow:hidden，纯 CSS 撑不出窗口外。
  // 进入聊天（首条内容或本地发送时）把 Tauri 窗口放大到能容纳弹性气泡的尺寸：
  // 宽 ~ min(480, 工作区宽 42%)，高 ~ min(560, 工作区高 56%)；底边锚定、显示器钳制，
  // 与 applyDeskSize 同一套读回+锚定逻辑。chatSizedRef 防抖：流式分片不重复 setSize。
  const enterChatSize = useCallback(async () => {
    if (!isTauriRuntime() || chatSizedRef.current) return;
    const cfg = profileRef.current;
    if (!cfg || !cfg.appearance.companion_enabled) return;
    chatSizedRef.current = true; // 先占位防抖，失败再回退
    try {
      const { getCurrentWindow, PhysicalPosition, PhysicalSize, availableMonitors } = await import('@tauri-apps/api/window');
      const win = getCurrentWindow();
      const [pos, size, scale] = await Promise.all([win.outerPosition(), win.outerSize(), win.scaleFactor()]);
      let monitors: { x: number; y: number; width: number; height: number }[] = [];
      try {
        monitors = (await availableMonitors()).map((m) => ({
          x: m.position.x,
          y: m.position.y,
          width: m.size.width,
          height: m.size.height,
        }));
      } catch {
        // 取不到显示器就不钳制
      }
      // 以当前窗口所在显示器的工作区（近似用整屏物理尺寸）按比例取目标尺寸。
      const host = monitors.length
        ? (monitors.find((m) => pos.x >= m.x && pos.x < m.x + m.width && pos.y >= m.y && pos.y < m.y + m.height) ?? monitors[0])
        : null;
      // 聊天态窗口随显示器比例自适应（大屏更大、小屏自动收、超大屏封顶），而非旧的
      // 恒等于 480×560 固定值。clamp 上限保证「不太占屏」；高度再保证立绘上方至少留出
      // ~220px 给气泡，免得大立绘（自定义形象 figureHeight 可达 ~430）把气泡压扁。
      const clampPx = (min: number, val: number, max: number) => Math.round(Math.max(min, Math.min(max, val)));
      const deskNow = getDeskSpecFor(cfg.character, customFigureMetaOf(cfg));
      const reservePx = deskNow.figureHeight + 84; // 与 CSS 变量 --companion-reserve 保持一致
      const screenW = host ? host.width / scale : 1920;
      const screenH = host ? host.height / scale : 1080;
      const targetLogical = {
        width: clampPx(360, screenW * 0.3, 560),
        height: clampPx(440, Math.max(screenH * 0.6, reservePx + 220), 720),
      };
      const target = { width: Math.round(targetLogical.width * scale), height: Math.round(targetLogical.height * scale) };
      // 已经够大（窗口比目标还大，如全身角色）就不动，避免缩小桌面伙伴画面。
      if (size.width >= target.width && size.height >= target.height) {
        chatSizedRef.current = false;
        return;
      }
      await win.setSize(new PhysicalSize(target.width, target.height));
      const achieved = await win.outerSize();
      if (achieved.width === size.width && achieved.height === size.height) {
        // WM 拒绝了 resize：撤销占位，下次还能再试。
        chatSizedRef.current = false;
        return;
      }
      // 底边锚定 + 显示器钳制：气泡在头顶向上长，底部图标不动，且不飞出屏。
      const next = placeResizedWindow({ x: pos.x, y: pos.y, width: size.width, height: size.height }, achieved, monitors);
      await win.setPosition(new PhysicalPosition(next.x, next.y));
      lastLocalMoveAt.current = Date.now();
    } catch (e) {
      console.error('companion chat size apply failed:', e);
      chatSizedRef.current = false;
    }
  }, []);

  // 退出聊天（气泡清空）→ 还原桌面伙伴原生尺寸。复用 applyDeskSize（底边锚定）。
  const exitChatSize = useCallback(async () => {
    if (!chatSizedRef.current) return;
    chatSizedRef.current = false;
    const cfg = profileRef.current;
    if (cfg) await applyDeskSize(cfg, { anchor: 'bottom' });
  }, [applyDeskSize]);

  // No companionId in the URL (direct open / web preview): fall back to the first
  // enabled companion in the registry (retry — the embedded backend may be booting).
  useEffect(() => {
    if (companionId) return;
    let disposed = false;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    const resolve = async (attempt: number) => {
      try {
        const companions = await ipcBridge.companion.listCompanions.invoke();
        if (disposed) return;
        const candidate = companions.find((p) => p.appearance.companion_enabled) ?? companions[0];
        if (candidate) setCompanionId(candidate.id);
      } catch (e) {
        console.error('companion id resolve failed:', e);
        if (!disposed && attempt < INIT_MAX_RETRIES) {
          retryTimer = setTimeout(() => void resolve(attempt + 1), INIT_RETRY_MS);
        }
      }
    };
    void resolve(0);
    return () => {
      disposed = true;
      if (retryTimer) clearTimeout(retryTimer);
    };
  }, [companionId]);

  // Initial load (with retry — the embedded backend may still be booting)
  // + WS subscriptions.
  useEffect(() => {
    // The transparent Tauri window still shows the SPA theme's opaque body
    // background — clear it for the lifetime of the companion route.
    const prevBodyBg = document.body.style.background;
    const prevHtmlBg = document.documentElement.style.background;
    document.body.style.background = 'transparent';
    document.documentElement.style.background = 'transparent';

    let disposed = false;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    if (!companionId) {
      // Wait for the fallback resolution effect; keep only the bg override.
      return () => {
        disposed = true;
        document.body.style.background = prevBodyBg;
        document.documentElement.style.background = prevHtmlBg;
      };
    }
    if (!isTauriRuntime()) {
      // Web fallback still loads the profile (+ embedded status) so the
      // preview shows the configured character with its real mood.
      void ipcBridge.companion.getCompanion
        .invoke({ companion_id: companionId })
        .then((p) => {
          if (disposed) return;
          setProfile(p);
          setMood((p.status?.mood as RabbitMood) || 'content');
        })
        .catch(() => {});
      return () => {
        disposed = true;
        document.body.style.background = prevBodyBg;
        document.documentElement.style.background = prevHtmlBg;
      };
    }
    const init = async (attempt: number) => {
      try {
        const [withStatus, news] = await Promise.all([
          ipcBridge.companion.getCompanion.invoke({ companion_id: companionId }),
          ipcBridge.companion.listSuggestions.invoke({ status: 'new', limit: 50 }),
        ]);
        if (disposed) return;
        setProfile(withStatus);
        setMood((withStatus.status?.mood as RabbitMood) || 'content');
        setUnread(news.length);
        setSuggestions(news);
        await applyWindowState(withStatus);
        await applyDeskSize(withStatus, { anchor: 'top-left' });
      } catch (e) {
        console.error('companion init failed:', e);
        // Without a loaded profile the companion stays invisible forever — retry.
        if (!disposed && attempt < INIT_MAX_RETRIES) {
          retryTimer = setTimeout(() => void init(attempt + 1), INIT_RETRY_MS);
        }
      }
    };
    void init(0);

    const unsubMood = ipcBridge.companion.onMoodChanged.on((evt) => {
      if (!isForCompanion(evt, companionId)) return;
      setMood((evt.mood as RabbitMood) || 'content');
    });
    const unsubLearnStart = ipcBridge.companion.onLearnStarted.on((evt) => {
      if (!isForCompanion(evt, companionId)) return;
      setActivity('thinking');
    });
    const unsubLearnDone = ipcBridge.companion.onLearnFinished.on((run) => {
      if (!isForCompanion(run, companionId)) return;
      setActivity('idle');
      if (run.summary) popBubble(run.summary);
    });
    const unsubSuggestion = ipcBridge.companion.onSuggestionCreated.on((s) => {
      if (!isForCompanion(s, companionId)) return;
      // Dedup against current list outside the state updater — updaters must
      // stay pure (StrictMode double-invokes them) and popBubble/setUnread
      // are side effects.
      const known = suggestionsRef.current.some((x) => x.id === s.id);
      if (known) return;
      setSuggestions((list) => (list.some((x) => x.id === s.id) ? list : [s, ...list]));
      setUnread((n) => n + 1);
      popBubble(`${s.title}`);
    });
    // A suggestion was decided anywhere (e.g. the owner accepted/dismissed it
    // in the main panel). Drop it from the bubble + fix the unread badge so the
    // two surfaces stay in sync. Guard on suggestionsRef so we only decrement
    // unread for items we were actually showing (StrictMode-safe: no side
    // effects inside the state updater).
    const unsubSuggestionDecided = ipcBridge.companion.onSuggestionDecided.on((s) => {
      if (!suggestionsRef.current.some((x) => x.id === s.id)) return;
      setSuggestions((list) => list.filter((x) => x.id !== s.id));
      setUnread((n) => Math.max(0, n - 1));
    });
    const unsubConfig = ipcBridge.companion.onConfigUpdated.on((evt) => {
      // companion.config-updated is shared across scopes: `scope === "shared"` for
      // the cross-companion config, `scope === companion_id` (payload = full profile) for
      // a per-companion profile change. Only our own companion's profile matters here.
      if (evt.scope !== companionId) return;
      const next = evt as unknown as ICompanionProfile;
      setProfile(next);
      // Skip reapplying the persisted position right after a local drag:
      // the broadcast echoes our own debounced save and would teleport the
      // window back mid-drag.
      const justMoved = Date.now() - lastLocalMoveAt.current < 2_000;
      void applyWindowState(next, { skipPosition: justMoved }).then(() => applyDeskSize(next));
    });
    // Our companion was deleted (from /nomi or the API): this window is orphaned —
    // close it. The main window's sync would close it too; doing it here keeps
    // the window autonomous (sync may not be running, e.g. main window gone).
    const unsubDeleted = ipcBridge.companion.onCompanionDeleted.on(({ companion_id }) => {
      if (companion_id !== companionId) return;
      void (async () => {
        try {
          const { getCurrentWindow } = await import('@tauri-apps/api/window');
          await getCurrentWindow().close();
        } catch (e) {
          console.error('companion window self-close failed:', e);
        }
      })();
    });
    // The companion just saved a memory of its own (private to it): a low-key
    // bubble note, but only when idle so it never clobbers an in-flight reply.
    // Editing/managing is a right-click away (打开记忆 → the scope-aware tab).
    const unsubMemoryCreated = ipcBridge.companion.onMemoryCreated.on((m) => {
      if (!companionId || m.scope_companion_id !== companionId) return;
      if (turnActiveRef.current) return;
      const brief = m.content.length > 40 ? `${m.content.slice(0, 40)}…` : m.content;
      popBubble(t('nomi.companion.memorySavedToast', { brief }));
    });
    // Streamed companion reply → live bubble updates. The companion thread is
    // a real nomi conversation; message.stream is a global broadcast, so we
    // filter to the active thread ONLY (no turn-ownership gate — see the
    // responseStream handler below: that gate dropped badcase-3 replies).
    const endTurn = (finalText?: string) => {
      turnActiveRef.current = false;
      setBubbleRunning(false);
      segmentsRef.current.clear();
      segmentOrderRef.current = [];
      setBubbleLoading(false);
      setRemoteHeader(null);
      if (finalText !== undefined) setBubble(finalText);
      armBubbleDismiss(BUBBLE_MS * 2, () => setBubble(''));
    };
    const joinedSegments = () =>
      segmentOrderRef.current
        .map((id) => segmentsRef.current.get(id) ?? '')
        .join('\n\n')
        .trim();
    // Remote IM turns: channel master conversations bound to this companion ride
    // the same message.stream broadcast with a `channel_platform` marker.
    // Render them into the bubble too — with an incoming-message header — so
    // an IM chat plays out on the companion instead of only in the sidebar
    // history. The local turn keeps priority: while the owner is chatting
    // here, remote turns only buffer (the conversation record is complete
    // either way); a newer remote turn replaces the slot (latest wins).
    const inQuiet = () => {
      const cfg = profileRef.current;
      return Boolean(cfg && inQuietHours(cfg.appearance.quiet_start, cfg.appearance.quiet_end));
    };
    const joinedRemote = (slot: NonNullable<typeof remoteTurnRef.current>) =>
      slot.order
        .map((id) => slot.segments.get(id) ?? '')
        .join('\n\n')
        .trim();
    /** Show the remote slot in the bubble; false when the local turn (or
     *  quiet hours) owns it. */
    const renderRemote = (): boolean => {
      const slot = remoteTurnRef.current;
      if (!slot || turnActiveRef.current || inQuiet()) {
        markRemoteRunning(false);
        return false;
      }
      setRemoteHeader({ platform: slot.platform, inbound: slot.inbound });
      const text = joinedRemote(slot);
      setBubbleLoading(false);
      setBubble(text || '…');
      markRemoteRunning(!slot.finished);
      return true;
    };
    const dismissRemoteLater = (ms: number) => {
      armBubbleDismiss(ms, () => {
        setBubble('');
        setRemoteHeader(null);
        setBubbleLoading(false);
        markRemoteRunning(false);
      });
    };
    const handleRemoteStream = (message: IResponseMessage, platform: string) => {
      let slot = remoteTurnRef.current;
      if (
        !slot ||
        slot.conversationId !== message.conversation_id ||
        (slot.finished && Boolean(message.msg_id) && !slot.segments.has(message.msg_id))
      ) {
        // Stream from a turn whose userCreated we never saw (window opened
        // mid-turn, a second IM chat took over, or a follow-up turn in the
        // same chat): open a fresh slot without the inbound text.
        slot = {
          conversationId: message.conversation_id,
          platform,
          inbound: '',
          segments: new Map(),
          order: [],
          finished: false,
        };
        remoteTurnRef.current = slot;
      }
      switch (message.type) {
        case 'content':
        case 'text': {
          const chunk = extractResponseTextChunk(message.data);
          if (!chunk || !message.msg_id) return;
          if (!slot.segments.has(message.msg_id)) slot.order.push(message.msg_id);
          slot.segments.set(
            message.msg_id,
            message.replace ? chunk : (slot.segments.get(message.msg_id) ?? '') + chunk
          );
          // A post-finish replace override only refreshes the visible text;
          // keep the (shorter) post-turn dismiss window.
          if (renderRemote()) dismissRemoteLater(slot.finished ? BUBBLE_MS * 2 : STREAM_STALL_MS);
          break;
        }
        case 'tool_group':
        case 'tool_call':
        case 'acp_tool_call':
          if (renderRemote()) dismissRemoteLater(STREAM_STALL_MS);
          break;
        case 'finish':
        case 'error': {
          slot.finished = true;
          markRemoteRunning(false);
          if (turnActiveRef.current || inQuiet()) return;
          const text = joinedRemote(slot);
          setRemoteHeader({ platform: slot.platform, inbound: slot.inbound });
          setBubbleLoading(false);
          setBubble(
            text ||
              (message.type === 'error'
                ? t(companionErrorKey(streamErrorCode(message.data)))
                : t('nomi.companion.done'))
          );
          dismissRemoteLater(BUBBLE_MS * 2);
          break;
        }
        default:
          break;
      }
    };
    // The visitor's inbound IM message opens the remote turn: show it in the
    // bubble header right away with a waiting indicator for the reply.
    const unsubUserCreated = ipcBridge.conversation.userCreated.on((evt) => {
      if (evt.hidden) return;
      if (!evt.companion || evt.companion_id !== companionId || !evt.channel_platform) return;
      remoteTurnRef.current = {
        conversationId: evt.conversation_id,
        platform: evt.channel_platform,
        inbound: evt.content,
        segments: new Map(),
        order: [],
        finished: false,
      };
      if (renderRemote()) dismissRemoteLater(STREAM_STALL_MS);
    });
    const unsubStream = ipcBridge.conversation.responseStream.on((message) => {
      if (message.hidden) return;
      // Remote IM turn fragments carry the channel_platform wire marker;
      // local companion threads never do.
      if (message.companion && message.companion_id === companionId && message.channel_platform) {
        handleRemoteStream(message, message.channel_platform);
        return;
      }
      // 本地伙伴回合（本宠的专属会话）。**按 companion_id marker 识别**，而非旧的
      // `message.conversation_id === activeThreadRef.current` 数字相等闸：后端在每条
      // 分片上都打了 companion / companion_id 标记（stream_relay.broadcast_stream_payload），
      // 与上面远程路径按 channel_platform 识别同理。旧的数字比较是这个 bug 反复发作的真凶——
      // companion_threads.conversation_id 是 TEXT，活动流 id 是 i64，两种表示一旦分叉就把每条
      // 分片静默丢弃 → 气泡只剩「…」不回显（即便 ipcBridge 边界已强转 number，这个跨表示
      // 比较仍是唯一脆弱点）。marker 与会话不带任何字符串/数字陷阱。activeThreadRef 退化为
      // 兜底匹配（应对极少数无 marker 的分片），并从 marker 反向同步，保证后续 stall/消散逻辑照常。
      const isOwnLocalTurn =
        message.companion === true && message.companion_id === companionId && !message.channel_platform;
      const matchesActiveThread =
        activeThreadRef.current != null && message.conversation_id === activeThreadRef.current;
      if (!isOwnLocalTurn && !matchesActiveThread) return;
      if (isOwnLocalTurn && message.conversation_id != null) activeThreadRef.current = message.conversation_id;
      // 用户已「× 忽略」本轮：压制后续所有分片（含 stop 异步生效前的残片），不再回闪气泡。
      if (bubbleDismissedRef.current) return;
      // badcase 3 修复：去掉原 `if (!turnActiveRef.current && !isFinalOverride) return;` 渲染闸。
      // turnActiveRef 仅用于驱动占位/消散定时器：内容到达=重置 stall；finish/error=收尾+消散。
      switch (message.type) {
        case 'content':
        case 'text': {
          const chunk = extractResponseTextChunk(message.data);
          if (!chunk || !message.msg_id) return;
          if (!segmentsRef.current.has(message.msg_id)) segmentOrderRef.current.push(message.msg_id);
          segmentsRef.current.set(
            message.msg_id,
            message.replace ? chunk : (segmentsRef.current.get(message.msg_id) ?? '') + chunk
          );
          const text = joinedSegments();
          setBubbleLoading(false);
          setBubble(text || '…');
          if (!turnActiveRef.current) {
            // 本轮已 finish/error 收尾，这是 middleware 改写后的 replace 终稿覆盖：
            // 只刷新可见文本 + 保持较短的消散窗口，不要再拉起 45s stall。
            armBubbleDismiss(BUBBLE_MS * 2, () => setBubble(''));
            return;
          }
          // 仍在流式中：内容到达即（重）拉 stall 安全网——finish 丢失时也会最终消散。
          armBubbleDismiss(STREAM_STALL_MS, () => endTurn(''));
          break;
        }
        case 'tool_group':
        case 'tool_call':
        case 'acp_tool_call': {
          // Tool activity: keep the bubble alive with a hint if no text yet.
          // P3-N1: the Browser tool gets a *specific* narration (navigate to
          // example.com / clicking / observing…) parsed from the event args
          // (decision ⑧, mirrors `BrowserTool::describe`); every other tool keeps
          // the generic `usingTools` placeholder. The stall/dismiss safety net
          // below is identical for both paths — narration only swaps the text.
          setBubbleLoading(false);
          const browser = browserNarrationFor(message.data);
          const hint = browser
            ? t(browser.key, { name: profileRef.current?.name || 'Nomi', ...browser.params })
            : t('nomi.companion.usingTools', { name: profileRef.current?.name || 'Nomi' });
          setBubble((prev) => (prev && prev !== '…' ? prev : hint));
          armBubbleDismiss(STREAM_STALL_MS, () => endTurn(''));
          break;
        }
        case 'permission':
        case 'acp_permission':
          // The bubble has no confirmation UI — route the user to the full
          // chat surface where MessagePermission renders.
          endTurn(t('nomi.companion.needsConfirm'));
          break;
        case 'finish': {
          // The engine emits exactly one finish per turn; close even when the
          // turn produced no prose (tool-only turn → friendly fallback). Keep
          // the segment buffers: a replace:true override may follow finish.
          const text = joinedSegments();
          const keepSegments = segmentsRef.current;
          const keepOrder = segmentOrderRef.current;
          endTurn(text || t('nomi.companion.done'));
          segmentsRef.current = keepSegments;
          segmentOrderRef.current = keepOrder;
          break;
        }
        case 'error': {
          // Keep any streamed text (mid-turn errors can be non-fatal noise);
          // only fall back when there's nothing to show — and make that fallback
          // actionable by mapping the provider error code (auth/network/rate/…)
          // instead of the generic "走神".
          const text = joinedSegments();
          endTurn(text || t(companionErrorKey(streamErrorCode(message.data))));
          break;
        }
        default:
          break;
      }
    });
    return () => {
      disposed = true;
      if (retryTimer) clearTimeout(retryTimer);
      document.body.style.background = prevBodyBg;
      document.documentElement.style.background = prevHtmlBg;
      unsubMood();
      unsubLearnStart();
      unsubLearnDone();
      unsubSuggestion();
      unsubSuggestionDecided();
      unsubConfig();
      unsubDeleted();
      unsubMemoryCreated();
      unsubUserCreated();
      unsubStream();
      if (bubbleTimer.current) clearTimeout(bubbleTimer.current);
    };
  }, [applyDeskSize, applyWindowState, companionId, popBubble, t]);

  // Persist the window position after drags (debounced onMoved).
  useEffect(() => {
    if (!isTauriRuntime() || !companionId) return;
    let unlisten: (() => void) | undefined;
    let timer: ReturnType<typeof setTimeout> | null = null;
    void (async () => {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      unlisten = await getCurrentWindow().onMoved(({ payload }) => {
        lastLocalMoveAt.current = Date.now();
        if (timer) clearTimeout(timer);
        timer = setTimeout(() => {
          lastLocalMoveAt.current = Date.now();
          // Merge-patch only this companion's position: never clobbers concurrent
          // edits (settings toggles in the main window) the way a full PUT does.
          void ipcBridge.companion.patchCompanion
            .invoke({ companion_id: companionId, patch: { appearance: { companion_x: payload.x, companion_y: payload.y } } })
            .then((saved) => {
              profileRef.current = saved;
              setProfile(saved);
            })
            .catch(() => {});
        }, 600);
      });
    })();
    return () => {
      unlisten?.();
      if (timer) clearTimeout(timer);
    };
  }, [companionId]);

  // 聊天模式窗口尺寸切换（弹性气泡，badcase 3）。仅随「有无气泡」这一布尔翻转触发，
  // 流式过程中气泡文本在两个非空值之间变化不会重跑本 effect —— 天然防抖，
  // 不会每个分片都 setSize。气泡出现→放大；气泡清空→还原桌面伙伴尺寸。
  const hasBubble = bubble.length > 0;
  useEffect(() => {
    if (!isTauriRuntime()) return;
    // 气泡或展开态 composer 都需要放大原生窗口才能撑得开。
    if (hasBubble || composerOpen) void enterChatSize();
    else void exitChatSize();
  }, [hasBubble, composerOpen, enterChatSize, exitChatSize]);

  const forceCompanionCapture = shouldCaptureWholeCompanionWindow({
    composerOpen,
    barRevealed,
    hasInput: Boolean(input),
    sending,
    showSuggestions,
    dragOver,
  });

  // 按区域点击穿透：默认整窗穿透，只有光标落在标了 data-companion-hit 的交互元素
  // （立绘 / 气泡 / 输入条 / 角标 / 建议）包围盒内时才捕获鼠标。删除了「整块透明
  // 窗口拦截底层点击」的遮罩感，又不动伙伴显示。onHoverChange 同步驱动「悬停才出现」的
  // 迷你输入条显隐（见 barRevealed）。
  // enabled 用「未显式停用」而非「已启用」：窗口在 Rust 创建后即 show()，配置(profile)
  // 还在拉取时窗口已可见，此刻就该穿透——否则启动数秒内整窗仍挡点击。停用→窗口 hide()→
  // companion_enabled=false→停轮询。
  useCompanionClickThrough({
    enabled: isTauriRuntime() && profile?.appearance.companion_enabled !== false,
    onHoverChange: handleCompanionHoverChange,
    // 明确交互态整窗捕获：建议/展开 composer，以及迷你输入条已显露或有草稿时，
    // 优先保证控件可点可聚焦；离开后由 bar reveal 延迟和状态清空恢复按区域穿透。
    captureAll: forceCompanionCapture,
    // 拖动期间冻结轮询：根除拖动中 setIgnoreCursorEvents 反复翻转引起的闪动。
    dragging,
  });

  // ---- 跟随主窗主题：跨窗口实时同步（亮/暗 data-theme + 氛围预设 customCss）----
  // 桌宠是独立窗口、不挂 Layout，configService 通知不跨窗。主窗切换亮/暗或氛围预设时
  // 经 Tauri 全局事件广播过来：亮/暗重应用 data-theme，customCss 重注入 <style>，
  // 使桌宠气泡/输入框（及功能按键）chrome 完整跟随主窗氛围。初始值在挂载时主动读取
  // 一次（应对桌宠晚于主窗打开的情况）；注入带透明保护，见 applyCustomCss。
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void configService
      .whenReady()
      .then(() => {
        if (!disposed) injectCompanionCustomCss((configService.get('customCss') as string) || '');
      })
      .catch(() => {});
    void import('@tauri-apps/api/event')
      .then(({ listen }) =>
        listen<ThemeSyncPayload>(THEME_SYNC_EVENT, (evt) => {
          const payload = evt.payload;
          const next = payload?.theme;
          if (next === 'light' || next === 'dark') {
            document.documentElement.setAttribute('data-theme', next);
            document.body.setAttribute('arco-theme', next);
          }
          if (payload?.customCss !== undefined) injectCompanionCustomCss(payload.customCss);
        })
      )
      .then((un) => {
        if (disposed) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // 让原生窗口标题跟随伙伴的自定义名字。建窗时只给了占位标题 "NomiFun"（见 main.rs），
  // 窗口虽 skip_taskbar，但 alt-tab / 屏幕阅读器 / macOS 窗口菜单仍会读到它——给每个
  // 伙伴专属称呼而非千篇一律的 "nomi"。初次加载与重命名（config-updated → setProfile）
  // 都会触发本 effect。
  useEffect(() => {
    if (!isTauriRuntime()) return;
    const name = profile?.name?.trim();
    if (!name) return;
    void import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => getCurrentWindow().setTitle(name))
      .catch(() => {});
  }, [profile?.name]);

  // ---- 气泡正文「粘底自动追尾」（持续对话感，无需用户手动滚动）----
  // 流式每来一段文本就把滚动视口钉到底部；用户一旦上滑读历史(离底 > 阈值)就停手不抢滚，
  // 滑回底部自动恢复追尾。换轮 / 中间件 replace 终稿覆盖(文本变短)会自动重新钉底。
  const onBubbleScroll = useCallback(() => {
    const el = bubbleScrollRef.current;
    if (!el) return;
    const distToBottom = el.scrollHeight - el.scrollTop - el.clientHeight;
    bubbleStickRef.current = distToBottom <= 28;
    // 顶部渐隐遮罩仅在向下滚动后出现，短消息首行不被切。
    el.classList.toggle('is-scrolled', el.scrollTop > 4);
  }, []);

  useLayoutEffect(() => {
    if (!bubble) {
      bubbleStickRef.current = true;
      bubblePrevLenRef.current = 0;
      return;
    }
    const el = bubbleScrollRef.current;
    if (!el) {
      bubblePrevLenRef.current = bubble.length;
      return;
    }
    if (bubble.length < bubblePrevLenRef.current) bubbleStickRef.current = true;
    bubblePrevLenRef.current = bubble.length;
    if (bubbleStickRef.current) el.scrollTop = el.scrollHeight;
    el.classList.toggle('is-scrolled', el.scrollTop > 4);
  }, [bubble]);

  // ResizeObserver 兜底：代码高亮 / 图片 / 公式 / Shadow DOM 延迟挂载会异步撑高内容，
  // 此时仍按粘底意图钉到底，避免追尾「慢半拍」。仅随气泡有无重建。
  useEffect(() => {
    const content = bubbleContentRef.current;
    const el = bubbleScrollRef.current;
    if (!content || !el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => {
      if (bubbleStickRef.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(content);
    return () => ro.disconnect();
  }, [hasBubble]);

  const startDrag = useCallback(async (e: React.MouseEvent) => {
    if (e.button !== 0 || !isTauriRuntime()) return;
    setDragging(true); // 冻结点击穿透轮询，根除拖动闪动
    let ended = false;
    let unlistenMoved: (() => void) | undefined;
    let silence: ReturnType<typeof setTimeout> | null = null;
    let safety: ReturnType<typeof setTimeout> | null = null;
    const end = () => {
      if (ended) return;
      ended = true;
      window.removeEventListener('pointerup', end);
      window.removeEventListener('mouseup', end);
      if (silence) clearTimeout(silence);
      if (safety) clearTimeout(safety);
      unlistenMoved?.();
      setDragging(false);
    };
    // 多信号界定拖动结束：松手(pointerup/mouseup) 为主；onMoved 停止 220ms 兜底
    // （模态移动循环下 mouseup 可能不达 webview）；6s 安全超时防全漏导致永久冻结。
    const bumpSilence = () => {
      if (silence) clearTimeout(silence);
      silence = setTimeout(end, 220);
    };
    window.addEventListener('pointerup', end);
    window.addEventListener('mouseup', end);
    safety = setTimeout(end, 6000);
    try {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      unlistenMoved = await getCurrentWindow().onMoved(bumpSilence);
      if (ended) {
        unlistenMoved();
        return;
      }
      await getCurrentWindow().startDragging();
    } catch {
      end(); // startDragging 抛错则立即解冻
    }
  }, []);

  const openMainAt = useCallback(async (path: string) => {
    if (!isTauriRuntime()) return;
    try {
      const { emitTo } = await import('@tauri-apps/api/event');
      const { Window } = await import('@tauri-apps/api/window');
      await emitTo('main', 'companion-navigate', path);
      const main = await Window.getByLabel('main');
      await main?.setFocus();
    } catch (e) {
      console.error('companion→main navigate failed:', e);
    }
  }, []);

  /** 解析（或幂等创建）该伙伴的唯一专属会话 id。单会话契约（FE2）：
   *  先读 getCompanionSession，有 id 直接用；否则 ensureCompanionSession 幂等创建。
   *  创建会要求 profile.model 已配置，否则后端返回 400 — 这里向上抛，由 sendChat
   *  捕获后给气泡一个简短提示并放弃本轮。多线程列表/新建/重命名/设活的旧 ipc 方法已废除。 */
  const ensureThread = useCallback(async (): Promise<number> => {
    if (!companionId) throw new Error('companion id not resolved yet');
    const active = await ipcBridge.companion.getCompanionSession.invoke({ companion_id: companionId });
    if (active.conversation_id != null) {
      activeThreadRef.current = active.conversation_id;
      return active.conversation_id;
    }
    // 无会话：幂等 ensure（未配置对话模型则后端 400，错误冒泡给 sendChat）。
    const created = await ipcBridge.companion.ensureCompanionSession.invoke({ companion_id: companionId });
    activeThreadRef.current = created.conversation_id;
    return created.conversation_id;
  }, [companionId]);

  // 粘贴图片 → 累积到 attachedFiles（路径），并自动展开 composer。
  const onPasteFilesAdded = useCallback((metas: FileMetadata[]) => {
    const paths = metas.map((m) => m.path).filter(Boolean);
    if (paths.length === 0) return;
    setAttachedFiles((prev) => [...prev, ...paths]);
    setComposerOpen(true);
  }, []);
  const { onPaste: onComposerPaste, onFocus: onComposerFocus } = usePasteService({
    supportedExts: imageExts,
    conversation_id: composerThreadId != null ? String(composerThreadId) : undefined,
    onFilesAdded: onPasteFilesAdded,
    source: 'sendbox',
  });

  /** 把若干本地路径中的图片追加为附件并展开 composer（附件按钮 / 拖拽共用单一入口）。 */
  const addImagePaths = useCallback((paths: string[]) => {
    const imgs = paths.filter((p) => imageExts.some((ext) => p.toLowerCase().endsWith(ext)));
    if (imgs.length === 0) return;
    setAttachedFiles((prev) => [...prev, ...imgs]);
    setComposerOpen(true);
  }, []);

  /** 附件按钮：打开「仅图片」的本地文件选择对话框（桌面端原生 dialog，直接拿绝对路径）。 */
  const pickImages = useCallback(() => {
    void ipcBridge.dialog.showOpen
      .invoke({
        properties: ['openFile', 'multiSelections'],
        filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg'] }],
      })
      .then((files) => {
        if (files && files.length > 0) addImagePaths(files);
      })
      .catch(() => {});
  }, [addImagePaths]);

  // 拖拽图片进桌宠窗：桌面端 OS 文件拖放经 Tauri 原生 onDragDropEvent 投递（webview 的
  // DOM ondrop 拿不到真实主机路径），这里取图片绝对路径追加为附件；enter/over 高亮、drop 落盘。
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void import('@tauri-apps/api/webview')
      .then(({ getCurrentWebview }) =>
        getCurrentWebview().onDragDropEvent((event) => {
          const p = event.payload as { type: string; paths?: string[] };
          if (p.type === 'enter' || p.type === 'over') setDragOver(true);
          else if (p.type === 'leave' || p.type === 'cancel') setDragOver(false);
          else if (p.type === 'drop') {
            setDragOver(false);
            if (Array.isArray(p.paths) && p.paths.length > 0) addImagePaths(p.paths);
          }
        })
      )
      .then((un) => {
        if (cancelled) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [addImagePaths]);

  /** 本地伙伴回合统一提交（迷你/展开共用）：文字 + 可选图片附件。流式回复照常入气泡。 */
  const submitTurn = useCallback(
    async (text: string, files: string[]) => {
      setSending(true);
      bubbleDismissedRef.current = false; // 新一轮：解除上一轮的「忽略」压制
      setBubbleLoading(true);
      segmentsRef.current.clear();
      segmentOrderRef.current = [];
      setRemoteHeader(null);
      markRemoteRunning(false);
      setBubble('…');
      // A pending dismiss timer (an earlier turn's fade-out or a remote bubble's
      // stall guard) would wipe the placeholder mid-send.
      if (bubbleTimer.current) clearTimeout(bubbleTimer.current);
      try {
        const conversation_id = await ensureThread();
        turnActiveRef.current = true;
        setBubbleRunning(true);
        // 把图片路径以 NOMIFUN_FILES_MARKER 形式嵌进 input（与 NomiSendBox 一致），
        // 这样 agent 才能看到附件；空 workspace 下绝对路径原样保留。
        const displayInput = files.length > 0 ? buildDisplayMessage(text, files, '') : text;
        await ipcBridge.conversation.sendMessage.invoke({
          input: displayInput,
          conversation_id,
          files: files.length > 0 ? files : undefined,
        });
        // Arm the stall timer only while nothing has streamed yet; once events
        // arrive, the stream handler owns bubbleTimer (re-armed per event).
        if (turnActiveRef.current && segmentsRef.current.size === 0) {
          armBubbleDismiss(STREAM_STALL_MS, () => {
            if (segmentsRef.current.size > 0) return; // stream started since
            turnActiveRef.current = false;
            setBubbleRunning(false);
            setBubbleLoading(false);
            setBubble('');
          });
        }
      } catch (e) {
        turnActiveRef.current = false;
        setBubbleRunning(false);
        setBubbleLoading(false);
        // 未配置对话模型时 ensureCompanionSession 返回 400 → 引导配置；其余后端错误按
        // error code 给可执行文案(鉴权/网络/限流/…),只有真正未知错误才退回通用兜底。
        if (isBackendHttpError(e) && e.status === 400) {
          setBubble(t('nomi.chat.modelMissing'));
        } else {
          const code = isBackendHttpError(e) ? e.code : '';
          setBubble(t(companionErrorKey(code)));
        }
        armBubbleDismiss(BUBBLE_MS, () => setBubble(''));
      } finally {
        setSending(false);
      }
    },
    [ensureThread, t, armBubbleDismiss, markRemoteRunning]
  );

  const sendChat = useCallback(() => {
    const text = input.trim();
    if (!text || sending) return;
    setInput('');
    void submitTurn(text, []);
  }, [input, sending, submitTurn]);

  const sendComposer = useCallback(() => {
    const text = composerText.trim();
    if ((!text && attachedFiles.length === 0) || sending) return;
    const files = attachedFiles;
    setComposerOpen(false);
    setComposerText('');
    setAttachedFiles([]);
    void submitTurn(text, files);
  }, [composerText, attachedFiles, sending, submitTurn]);

  /** 展开 composer：把迷你输入迁入 composer（并清空迷你框，单一来源防重复发送）、
   *  预解析会话 id（供粘贴上传关联，best-effort）。 */
  const openComposer = useCallback(() => {
    setComposerOpen(true);
    setComposerText((prev) => (prev ? prev : input));
    setInput('');
    void ensureThread()
      .then(setComposerThreadId)
      .catch(() => {});
  }, [input, ensureThread]);

  /** 收起 composer：丢弃未发送的附件，避免隐形累积。 */
  const collapseComposer = useCallback(() => {
    setComposerOpen(false);
    setAttachedFiles([]);
  }, []);

  /** 气泡「× 忽略」：立即隐藏气泡；若仍在生成（本地或远程 IM 回合）则一并停掉后端，
   *  避免无谓生成与残片回闪。 */
  const dismissBubble = useCallback(() => {
    // 远程 IM 自动回复进行中：× 一并停掉远程会话生成。
    if (remoteRunningRef.current && remoteTurnRef.current) {
      void ipcBridge.conversation.stop
        .invoke({ conversation_id: remoteTurnRef.current.conversationId })
        .catch(() => {});
      remoteTurnRef.current.finished = true;
      markRemoteRunning(false);
    }
    const cid = activeThreadRef.current;
    if (turnActiveRef.current && cid != null) {
      void ipcBridge.conversation.stop.invoke({ conversation_id: cid }).catch(() => {});
    }
    bubbleDismissedRef.current = true; // 压制本轮后续残片，防回闪
    turnActiveRef.current = false;
    setBubbleRunning(false);
    setBubbleLoading(false);
    clearBubbleTimer();
    setRemoteHeader(null);
    setBubble('');
  }, [clearBubbleTimer, markRemoteRunning]);

  /** 气泡「□ 打断」：停掉后端生成（本地或远程 IM 回合），保留已显示的部分文本，按常规计时淡出。 */
  const interruptReply = useCallback(() => {
    // 远程 IM 自动回复优先：打断作用于远程会话（点 6）。
    if (remoteRunningRef.current && remoteTurnRef.current) {
      void ipcBridge.conversation.stop
        .invoke({ conversation_id: remoteTurnRef.current.conversationId })
        .catch(() => {});
      remoteTurnRef.current.finished = true;
      markRemoteRunning(false);
      armBubbleDismiss(BUBBLE_MS, () => {
        setBubble('');
        setRemoteHeader(null);
      });
      return;
    }
    const cid = activeThreadRef.current;
    if (cid != null) {
      void ipcBridge.conversation.stop.invoke({ conversation_id: cid }).catch(() => {});
    }
    turnActiveRef.current = false;
    setBubbleRunning(false);
    setBubbleLoading(false);
    armBubbleDismiss(BUBBLE_MS, () => setBubble(''));
  }, [armBubbleDismiss, markRemoteRunning]);

  const hideCompanion = useCallback(async () => {
    if (!companionId) return;
    const next = await ipcBridge.companion.patchCompanion
      .invoke({ companion_id: companionId, patch: { appearance: { companion_enabled: false } } })
      .catch(() => null);
    if (next) {
      setProfile(next);
      await applyWindowState(next);
      // No-op while disabling (companion_enabled guard) — re-show resizing flows
      // through the config-updated path; kept for call-site symmetry.
      await applyDeskSize(next);
    }
  }, [applyDeskSize, applyWindowState, companionId]);

  const clearUnreadSuggestions = useCallback(() => {
    setUnread(0);
    setShowSuggestions(false);
    // Dismiss server-side too — otherwise the badge resurrects on the next
    // window load.
    const pending = suggestionsRef.current;
    setSuggestions([]);
    void Promise.allSettled(
      pending.map((s) => ipcBridge.companion.decideSuggestion.invoke({ id: s.id, accept: false }))
    );
  }, []);

  const runMenuAction = useCallback(
    (action: CompanionMenuAction) => {
      if (action === 'open-chat') {
        // 聊天已迁进「会话」：解析（幂等 ensure）该伙伴的唯一会话并在主窗口打开标准
        // /conversation/:id（旧的 /nomi?tab=chat 已废除）。未配置对话模型时 ensureThread
        // 返回 400 → 回退到管理中心总览引导配置。
        void (async () => {
          try {
            const cid = await ensureThread();
            await openMainAt(`/conversation/${cid}`);
          } catch {
            await openMainAt(
              companionId ? `/nomi?companion=${encodeURIComponent(companionId)}&tab=overview` : '/nomi'
            );
          }
        })();
        return;
      }
      if (action === 'open-memories') {
        // Lands on the scope-aware Memories tab for this companion (shared +
        // its own private). Memories live in the companion domain now.
        void openMainAt(
          companionId ? `/nomi?companion=${encodeURIComponent(companionId)}&tab=memories` : '/nomi?tab=memories'
        );
        return;
      }
      if (action === 'open-config') {
        void openMainAt(companionId ? `/nomi?companion=${encodeURIComponent(companionId)}&tab=settings` : '/nomi');
        return;
      }
      if (action === 'clear-unread') {
        clearUnreadSuggestions();
        return;
      }
      void hideCompanion();
    },
    [clearUnreadSuggestions, companionId, ensureThread, hideCompanion, openMainAt]
  );

  const openNativeContextMenu = useCallback(async () => {
    const entries = buildCompanionMenuEntries({ name: profileRef.current?.name || 'Nomi', t });
    try {
      const [{ Menu }, { getCurrentWindow }] = await Promise.all([
        import('@tauri-apps/api/menu'),
        import('@tauri-apps/api/window'),
      ]);
      const menu = await Menu.new({
        items: entries.map((entry) => ({
          id: `companion:${entry.action}`,
          text: entry.text,
          action: () => runMenuAction(entry.action),
        })),
      });
      await menu.popup(undefined, getCurrentWindow());
    } catch (e) {
      console.error('companion native context menu failed:', e);
    }
  }, [runMenuAction, t]);

  const clickSuggestion = useCallback(
    async (s: ICompanionSuggestion) => {
      // Click = "take me to handle it", NOT accept. We don't decide here (no
      // silent accept, no xp): the suggestion stays pending until the owner
      // explicitly accepts/dismisses it in the panel, and the resulting
      // companion.suggestion-decided event then syncs this bubble.
      setShowSuggestions(false);
      await openMainAt(s.action?.to || '/nomi?tab=suggestions');
    },
    [openMainAt]
  );

  const desk = getDeskSpecFor(profile?.character, customFigureMetaOf(profile));

  // 反应控件：□ 打断（本地或远程 IM 回合生成中）+ × 忽略（有气泡时）。从气泡上移到输入条
  // 发送按钮右侧的固定位置——气泡随内容伸缩、原位置动态难点中，固定常驻更易选中（点 5）。
  const reactionControls = (
    <>
      {(bubbleRunning || remoteRunning) && (
        <button
          type='button'
          className='nomi-companion-reaction'
          title={t('nomi.companion.interrupt')}
          onClick={(e) => {
            e.stopPropagation();
            interruptReply();
          }}
        >
          <svg width='10' height='10' viewBox='0 0 10 10' aria-hidden='true'>
            <rect x='1.5' y='1.5' width='7' height='7' rx='1.5' fill='currentColor' />
          </svg>
        </button>
      )}
      {bubble && (
        <button
          type='button'
          className='nomi-companion-reaction'
          title={t('nomi.companion.dismiss')}
          onClick={(e) => {
            e.stopPropagation();
            dismissBubble();
          }}
        >
          <svg width='10' height='10' viewBox='0 0 10 10' aria-hidden='true'>
            <path d='M1.6 1.6 L8.4 8.4 M8.4 1.6 L1.6 8.4' stroke='currentColor' strokeWidth='1.6' strokeLinecap='round' />
          </svg>
        </button>
      )}
    </>
  );

  if (!isTauriRuntime()) {
    return (
      <div className='nomi-companion-web-hint'>
        <CompanionAvatar
          character={profile?.character}
          mood={mood}
          activity={activity}
          size={120}
          companionId={companionId ?? undefined}
          customFigure={customFigureMetaOf(profile)}
        />
        <div>{t('nomi.companion.desktopOnly')}</div>
      </div>
    );
  }

  return (
    <div
      className='nomi-companion-window'
      // 气泡可用高度 = 100vh − 预留（立绘高 + 输入条/边距）。让正文吃满窗口剩余空间又不压到立绘。
      style={{ '--companion-reserve': `${desk.figureHeight + 84}px` } as React.CSSProperties}
      onContextMenu={(e) => {
        e.preventDefault();
        // 同步捕获：不等下个轮询 tick，先确保这次右键不会被透明穿透状态影响。
        if (isTauriRuntime()) {
          void import('@tauri-apps/api/window')
            .then(({ getCurrentWindow }) => getCurrentWindow().setIgnoreCursorEvents(false))
            .catch(() => {});
          void openNativeContextMenu();
        }
      }}
    >
      {bubble && (
        <div
          className={`nomi-companion-bubble ${bubbleLoading ? 'nomi-companion-bubble--loading' : ''}`}
          data-companion-hit
          onMouseEnter={() => {
            // 悬停即暂停消散——长回复不会没读完就消失（点 1）。
            bubbleHoveredRef.current = true;
            clearBubbleTimer();
          }}
          onMouseLeave={() => {
            bubbleHoveredRef.current = false;
            // 移开后重新计时；仍在生成时交回流式逻辑（下一片会重排 stall），不强行收尾。
            if (bubble && !bubbleRunning && !remoteRunningRef.current) {
              armBubbleDismiss(BUBBLE_MS, () => {
                setBubble('');
                setRemoteHeader(null);
              });
            }
          }}
        >
          {remoteHeader && (
            <div className='nomi-companion-bubble__remote'>
              {CHANNEL_LOGOS[remoteHeader.platform] && (
                <img src={CHANNEL_LOGOS[remoteHeader.platform]} alt={remoteHeader.platform} title={remoteHeader.platform} />
              )}
              <span>{remoteHeader.inbound || t('nomi.companion.remoteIncoming')}</span>
            </div>
          )}
          <div className='nomi-companion-bubble__scroll' ref={bubbleScrollRef} onScroll={onBubbleScroll}>
            <div className='nomi-companion-bubble__content' ref={bubbleContentRef}>
              {bubble === '…' ? (
                <span className='nomi-companion-bubble__typing' aria-label='…'>
                  <i />
                  <i />
                  <i />
                </span>
              ) : (
                <MarkdownView hiddenCodeCopyButton>{bubble}</MarkdownView>
              )}
            </div>
          </div>
        </div>
      )}
      <div className='nomi-companion-stage'>
        {unread > 0 && (
          <div
            className='nomi-companion-badge'
            data-companion-hit
            onClick={(e) => {
              e.stopPropagation();
              setShowSuggestions((v) => !v);
            }}
          >
            {unread > 99 ? '99+' : unread}
          </div>
        )}
        {showSuggestions && suggestions.length > 0 && (
          <div className='nomi-companion-suggestions' data-companion-hit onClick={(e) => e.stopPropagation()}>
            {suggestions.map((s) => (
              <div key={s.id} className='nomi-companion-suggestions__item' onClick={() => void clickSuggestion(s)}>
                <div className='nomi-companion-suggestions__title'>{s.title}</div>
                <div className='nomi-companion-suggestions__body'>{s.body}</div>
              </div>
            ))}
          </div>
        )}
        <div
          ref={figureHitRef}
          className='nomi-companion-figure-hit'
          data-companion-hit
          onMouseDown={(e) => void startDrag(e)}
        >
          <CompanionAvatar
            character={profile?.character}
            mood={mood}
            activity={activity}
            size={desk.figureHeight}
            companionId={companionId ?? undefined}
            customFigure={customFigureMetaOf(profile)}
            figureHitRef={figureHitRef}
          />
        </div>
      </div>
      {composerOpen ? (
        <div
          className={`nomi-companion-composer ${dragOver ? 'is-dragover' : ''}`}
          data-companion-hit
          onClick={(e) => e.stopPropagation()}
        >
          {attachedFiles.length > 0 && (
            <div className='nomi-companion-composer__thumbs'>
              {attachedFiles.map((path, i) => (
                <div key={`${path}-${i}`} className='nomi-companion-composer__thumb'>
                  <LocalImageView src={path} alt='' />
                  <button
                    className='nomi-companion-composer__thumb-x'
                    title={t('nomi.companion.collapse')}
                    onClick={() => setAttachedFiles((prev) => prev.filter((_, idx) => idx !== i))}
                  >
                    ×
                  </button>
                </div>
              ))}
            </div>
          )}
          <textarea
            className='nomi-companion-composer__input'
            value={composerText}
            placeholder={t('nomi.companion.chatPlaceholder', { name: profile?.name || 'Nomi' })}
            autoFocus
            onChange={(e) => setComposerText(e.target.value)}
            onFocus={onComposerFocus}
            onPaste={onComposerPaste}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                void sendComposer();
              }
            }}
          />
          <div className='nomi-companion-composer__bar'>
            <button
              type='button'
              className='nomi-companion-composer__attach'
              title={t('nomi.companion.attachImage')}
              disabled={sendboxUpload.isUploading}
              onClick={pickImages}
            >
              {sendboxUpload.isUploading ? (
                <span className='nomi-companion-spinner' aria-hidden='true' />
              ) : (
                <svg
                  width='16'
                  height='16'
                  viewBox='0 0 24 24'
                  fill='none'
                  stroke='currentColor'
                  strokeWidth='2'
                  strokeLinecap='round'
                  strokeLinejoin='round'
                >
                  <path d='M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48' />
                </svg>
              )}
            </button>
            <div className='spacer' />
            <button className='nomi-companion-composer__ghost' onClick={collapseComposer}>
              {t('nomi.companion.collapse')}
            </button>
            <button
              className='nomi-companion-composer__send'
              disabled={(!composerText.trim() && attachedFiles.length === 0) || sending || sendboxUpload.isUploading}
              onClick={() => void sendComposer()}
            >
              {t('nomi.companion.send')}
            </button>
            {reactionControls}
          </div>
        </div>
      ) : (
        <div
          className={`nomi-companion-chatbar ${input || sending || barRevealed ? 'is-active' : ''}`}
          // 常驻命中候选；隐藏态 pointer-events:none 会被 companionHitTarget 跳过，
          // CSS hover 先显示但 React reveal 尚未赶上时也不会出现「看得到却穿透」。
          data-companion-hit
        >
          <input
            value={input}
            placeholder={t('nomi.companion.chatPlaceholder', { name: profile?.name || 'Nomi' })}
            onChange={(e) => setInput(e.target.value)}
            onPaste={onComposerPaste}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void sendChat();
            }}
          />
          <div className='nomi-companion-iconbtn' title={t('nomi.companion.expand')} onClick={openComposer}>
            <svg
              width='14'
              height='14'
              viewBox='0 0 24 24'
              fill='none'
              stroke='currentColor'
              strokeWidth='2'
              strokeLinecap='round'
              strokeLinejoin='round'
            >
              <polyline points='15 3 21 3 21 9' />
              <polyline points='9 21 3 21 3 15' />
              <line x1='21' y1='3' x2='14' y2='10' />
              <line x1='3' y1='21' x2='10' y2='14' />
            </svg>
          </div>
          <button
            className='nomi-companion-send'
            disabled={!input.trim() || sending}
            onClick={() => void sendChat()}
          >
            {t('nomi.companion.send')}
          </button>
          {reactionControls}
        </div>
      )}
    </div>
  );
};

export default CompanionPage;
