import { useCallback, useEffect, useReducer, useRef } from 'react';
import type React from 'react';
import type { ICompanionSuggestion } from '@/common/adapter/ipcBridge';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import { configService } from '@/common/config/configService';
import { chooseDetachedMemoryPanelLayout, type DetachedMonitor } from './detachedMemoryPanelGeometry';
import {
  MEMORY_PANEL_EVENTS,
  initialMemoryPanelState,
  memoryPanelReducer,
  memoryPanelToggleIntent,
  nextMemoryPanelRequestId,
  shouldCloseMemoryPanelForOwnerGeometryChange,
  type MemoryPanelAction,
  type MemoryPanelActionAckPayload,
  type MemoryPanelActivatePayload,
  type MemoryPanelClosePayload,
  type MemoryPanelClosedPayload,
  type MemoryPanelCloseReason,
  type MemoryPanelMeasuredPayload,
  type MemoryPanelPhase,
  type MemoryPanelReadyPayload,
  type MemoryPanelSnapshotPayload,
} from './memoryPanelProtocol';
import { emitToMemoryPanel, hideMemoryPanelWindow, listenCurrentWindow, placeMemoryPanelWindow, prepareMemoryPanelWindow, showMemoryPanelWindow } from './memoryPanelShell';

export interface DetachedMemoryPanelController { phase: MemoryPanelPhase; isExpanded: boolean; toggle(): void; close(reason?: MemoryPanelCloseReason): void }

export function useDetachedMemoryPanel(options: {
  companionId: string | null;
  suggestions: ICompanionSuggestion[];
  onActivate: (suggestion: ICompanionSuggestion) => Promise<void>;
  onFallback: () => Promise<void>;
  badgeRef: React.RefObject<HTMLButtonElement | null>;
}): DetachedMemoryPanelController {
  const [state, dispatch] = useReducer(memoryPanelReducer, initialMemoryPanelState);
  const stateRef = useRef(state);
  const suggestionsRef = useRef(options.suggestions);
  const activateRef = useRef(options.onActivate);
  const fallbackRef = useRef(options.onFallback);
  const probeTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const ownerWindowLabelRef = useRef('');
  suggestionsRef.current = options.suggestions;
  activateRef.current = options.onActivate;
  fallbackRef.current = options.onFallback;

  const transition = useCallback((action: MemoryPanelAction) => {
    stateRef.current = memoryPanelReducer(stateRef.current, action);
    dispatch(action);
  }, []);

  const stopProbe = useCallback(() => {
    if (probeTimerRef.current) clearInterval(probeTimerRef.current);
    probeTimerRef.current = null;
  }, []);

  const close = useCallback((reason: MemoryPanelCloseReason = 'toggle') => {
    const current = stateRef.current;
    if (!current.requestId || current.phase === 'closed' || current.phase === 'closing') return;
    if (current.phase === 'preparing' || current.phase === 'opening') {
      stopProbe();
      void hideMemoryPanelWindow(current.requestId);
      transition({ type: 'closed', requestId: current.requestId });
      return;
    }
    transition({ type: 'request-close', requestId: current.requestId, reason });
    const payload: MemoryPanelClosePayload = { requestId: current.requestId, reason };
    void emitToMemoryPanel(MEMORY_PANEL_EVENTS.close, payload);
  }, [stopProbe, transition]);

  const open = useCallback(async () => {
    if (!isTauriRuntime() || !options.companionId || suggestionsRef.current.length === 0) return;
    const requestId = nextMemoryPanelRequestId(options.companionId);
    transition({ type: 'begin', requestId, ownerCompanionId: options.companionId });
    try {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      ownerWindowLabelRef.current = getCurrentWindow().label;
      await prepareMemoryPanelWindow();
      let attempts = 0;
      const sendProbe = () => {
        attempts += 1;
        if (stateRef.current.requestId !== requestId) {
          stopProbe();
          return;
        }
        if (attempts > 30) {
          stopProbe();
          transition({ type: 'closed', requestId });
          void fallbackRef.current();
          return;
        }
        void emitToMemoryPanel(MEMORY_PANEL_EVENTS.probe, { requestId, ownerWindowLabel: ownerWindowLabelRef.current });
      };
      sendProbe();
      probeTimerRef.current = setInterval(sendProbe, 60);
    } catch {
      if (stateRef.current.requestId !== requestId) return;
      transition({ type: 'closed', requestId });
      await fallbackRef.current();
    }
  }, [options.companionId, stopProbe, transition]);

  const toggle = useCallback(() => {
    if (memoryPanelToggleIntent(stateRef.current.phase) === 'open') void open();
    else close('toggle');
  }, [close, open]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void Promise.all([
      listenCurrentWindow<MemoryPanelReadyPayload>(MEMORY_PANEL_EVENTS.ready, (payload) => {
        const current = stateRef.current;
        if (payload.requestId !== current.requestId || !current.ownerCompanionId || current.phase !== 'preparing') return;
        stopProbe();
        const theme = document.documentElement.getAttribute('data-theme') === 'dark' ? 'dark' : 'light';
        const snapshot: MemoryPanelSnapshotPayload = { requestId: payload.requestId, ownerCompanionId: current.ownerCompanionId, ownerWindowLabel: ownerWindowLabelRef.current, suggestions: suggestionsRef.current, theme, customCss: String(configService.get('customCss') || '') };
        void emitToMemoryPanel(MEMORY_PANEL_EVENTS.snapshot, snapshot);
      }),
      listenCurrentWindow<MemoryPanelMeasuredPayload>(MEMORY_PANEL_EVENTS.measured, (payload) => {
        const current = stateRef.current;
        if (payload.requestId !== current.requestId || !current.ownerCompanionId || current.phase !== 'preparing') return;
        const ownerCompanionId = current.ownerCompanionId;
        transition({ type: 'opening', requestId: payload.requestId });
        void (async () => {
          const { availableMonitors, getCurrentWindow } = await import('@tauri-apps/api/window');
          const win = getCurrentWindow();
          const [position, size, monitors] = await Promise.all([win.outerPosition(), win.outerSize(), availableMonitors()]);
          if (stateRef.current.requestId !== payload.requestId || stateRef.current.phase !== 'opening') return;
          const mapped: DetachedMonitor[] = monitors.map((monitor) => ({ id: `${monitor.name ?? 'monitor'}:${monitor.position.x}:${monitor.position.y}:${monitor.scaleFactor}`, bounds: { x: monitor.position.x, y: monitor.position.y, width: monitor.size.width, height: monitor.size.height }, workArea: { x: monitor.workArea.position.x, y: monitor.workArea.position.y, width: monitor.workArea.size.width, height: monitor.workArea.size.height }, scaleFactor: monitor.scaleFactor }));
          const layout = chooseDetachedMemoryPanelLayout({ anchor: { x: position.x, y: position.y, width: size.width, height: size.height }, monitors: mapped, logicalPanel: { width: payload.width, height: payload.height } });
          if (layout.kind === 'fallback') { transition({ type: 'closed', requestId: payload.requestId }); await fallbackRef.current(); return; }
          await placeMemoryPanelWindow({ requestId: payload.requestId, ownerCompanionId, rect: layout.panelRect });
          if (stateRef.current.requestId !== payload.requestId || stateRef.current.phase !== 'opening') {
            await hideMemoryPanelWindow(payload.requestId);
            return;
          }
          await emitToMemoryPanel(MEMORY_PANEL_EVENTS.present, { requestId: payload.requestId, placement: layout.placement });
          if (stateRef.current.requestId !== payload.requestId || stateRef.current.phase !== 'opening') {
            await hideMemoryPanelWindow(payload.requestId);
            return;
          }
          const shown = await showMemoryPanelWindow({ requestId: payload.requestId, ownerCompanionId });
          if (!shown) { transition({ type: 'closed', requestId: payload.requestId }); return; }
          if (stateRef.current.requestId !== payload.requestId || stateRef.current.phase !== 'opening') {
            await hideMemoryPanelWindow(payload.requestId);
            return;
          }
          transition({ type: 'opened', requestId: payload.requestId });
          await emitToMemoryPanel(MEMORY_PANEL_EVENTS.visible, { requestId: payload.requestId });
        })().catch(async () => {
          await hideMemoryPanelWindow(payload.requestId).catch(() => false);
          if (stateRef.current.requestId !== payload.requestId) return;
          transition({ type: 'closed', requestId: payload.requestId });
          await fallbackRef.current();
        });
      }),
      listenCurrentWindow<MemoryPanelActivatePayload>(MEMORY_PANEL_EVENTS.activate, (payload) => {
        if (payload.requestId !== stateRef.current.requestId) return;
        const suggestion = suggestionsRef.current.find((item) => item.id === payload.suggestionId);
        void (async () => {
          let ok = false;
          try {
            if (suggestion) { await activateRef.current(suggestion); ok = true; }
          } catch {
            ok = false;
          } finally {
            const ack: MemoryPanelActionAckPayload = { requestId: payload.requestId, suggestionId: payload.suggestionId, ok };
            await emitToMemoryPanel(MEMORY_PANEL_EVENTS.actionAck, ack);
          }
        })();
      }),
      listenCurrentWindow<MemoryPanelClosedPayload>(MEMORY_PANEL_EVENTS.closed, (payload) => {
        if (payload.requestId !== stateRef.current.requestId) return;
        transition({ type: 'closed', requestId: payload.requestId });
        if (payload.restoreFocus) requestAnimationFrame(() => options.badgeRef.current?.focus());
      }),
    ]).then((items) => disposed ? items.forEach((unlisten) => unlisten()) : unlisteners.push(...items));
    return () => { disposed = true; stopProbe(); unlisteners.forEach((unlisten) => unlisten()); };
  }, [options.badgeRef, stopProbe, transition]);

  useEffect(() => { if (options.suggestions.length > 0 && isTauriRuntime()) void prepareMemoryPanelWindow(); }, [options.suggestions.length]);
  useEffect(() => {
    const current = stateRef.current;
    if (current.phase !== 'open' || !current.requestId || !current.ownerCompanionId || !ownerWindowLabelRef.current) return;
    const theme = document.documentElement.getAttribute('data-theme') === 'dark' ? 'dark' : 'light';
    const snapshot: MemoryPanelSnapshotPayload = {
      requestId: current.requestId,
      ownerCompanionId: current.ownerCompanionId,
      ownerWindowLabel: ownerWindowLabelRef.current,
      suggestions: options.suggestions,
      theme,
      customCss: String(configService.get('customCss') || ''),
    };
    void emitToMemoryPanel(MEMORY_PANEL_EVENTS.snapshot, snapshot);
  }, [options.suggestions]);
  useEffect(() => { if (options.suggestions.length === 0) close('empty'); }, [close, options.suggestions.length]);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    const closeForGeometryChange = () => {
      if (shouldCloseMemoryPanelForOwnerGeometryChange(stateRef.current.phase)) close('owner-invalid');
    };
    void import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => {
        const win = getCurrentWindow();
        return Promise.all([
          win.onMoved(closeForGeometryChange),
          win.onResized(closeForGeometryChange),
          win.onScaleChanged(closeForGeometryChange),
        ]);
      })
      .then((nextUnlisteners) => {
        if (disposed) nextUnlisteners.forEach((unlisten) => unlisten());
        else unlisteners.push(...nextUnlisteners);
      })
      .catch(() => {});
    return () => { disposed = true; unlisteners.forEach((unlisten) => unlisten()); };
  }, [close]);
  useEffect(() => () => close('owner-invalid'), [close]);

  return { phase: state.phase, isExpanded: state.phase !== 'closed', toggle, close };
}
