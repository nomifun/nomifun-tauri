import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import { injectCompanionCustomCss } from '@/renderer/utils/theme/applyCustomCss';
import {
  MEMORY_PANEL_EVENTS,
  type MemoryPanelActionAckPayload,
  type MemoryPanelActivatePayload,
  type MemoryPanelClosePayload,
  type MemoryPanelClosedPayload,
  type MemoryPanelMeasuredPayload,
  type MemoryPanelPhase,
  type MemoryPanelPresentPayload,
  type MemoryPanelProbePayload,
  type MemoryPanelReadyPayload,
  type MemoryPanelSnapshotPayload,
  type MemoryPanelVisiblePayload,
} from '@/renderer/pages/companion/memoryPanelProtocol';
import { emitToWindow, hideMemoryPanelWindow, listenCurrentWindow } from '@/renderer/pages/companion/memoryPanelShell';
import './memoryPanel.css';

const MemoryPanelPage: React.FC = () => {
  const { t } = useTranslation();
  const [snapshot, setSnapshot] = useState<MemoryPanelSnapshotPayload | null>(null);
  const [phase, setPhase] = useState<MemoryPanelPhase>('closed');
  const [placement, setPlacement] = useState('above');
  const cardRef = useRef<HTMLElement | null>(null);
  const firstItemRef = useRef<HTMLButtonElement | null>(null);
  const snapshotRef = useRef(snapshot);
  const phaseRef = useRef(phase);
  const activationPendingRef = useRef(false);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const finishClose = useCallback(async (payload: MemoryPanelClosePayload) => {
    const current = snapshotRef.current;
    if (!current || current.requestId !== payload.requestId || phaseRef.current === 'closing') return;
    setPhase('closing');
    phaseRef.current = 'closing';
    activationPendingRef.current = false;
    if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    const reduced = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches ?? false;
    closeTimerRef.current = setTimeout(async () => {
      await hideMemoryPanelWindow(payload.requestId).catch(() => false);
      if (snapshotRef.current?.requestId !== payload.requestId) return;
      const closed: MemoryPanelClosedPayload = { ...payload, restoreFocus: payload.reason === 'escape' };
      closeTimerRef.current = null;
      snapshotRef.current = null;
      phaseRef.current = 'closed';
      setSnapshot(null);
      setPhase('closed');
      await emitToWindow(current.ownerWindowLabel, MEMORY_PANEL_EVENTS.closed, closed).catch(() => {});
    }, reduced ? 0 : 140);
  }, []);

  useLayoutEffect(() => {
    const current = snapshot;
    const card = cardRef.current;
    if (!current || !card || phaseRef.current === 'open') return;
    const rect = card.getBoundingClientRect();
    const measured: MemoryPanelMeasuredPayload = {
      requestId: current.requestId,
      ownerWindowLabel: current.ownerWindowLabel,
      width: Math.ceil(rect.width),
      height: Math.ceil(rect.height),
    };
    void emitToWindow(current.ownerWindowLabel, MEMORY_PANEL_EVENTS.measured, measured);
  }, [snapshot]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void Promise.all([
      listenCurrentWindow<MemoryPanelProbePayload>(MEMORY_PANEL_EVENTS.probe, (payload) => {
        const ready: MemoryPanelReadyPayload = payload;
        void emitToWindow(payload.ownerWindowLabel, MEMORY_PANEL_EVENTS.ready, ready);
      }),
      listenCurrentWindow<MemoryPanelSnapshotPayload>(MEMORY_PANEL_EVENTS.snapshot, (payload) => {
        const previous = snapshotRef.current;
        const sameRequest = previous?.requestId === payload.requestId;
        if (previous && !sameRequest) {
          const closed: MemoryPanelClosedPayload = { requestId: previous.requestId, reason: 'owner-invalid', restoreFocus: false };
          void emitToWindow(previous.ownerWindowLabel, MEMORY_PANEL_EVENTS.closed, closed);
          void hideMemoryPanelWindow(previous.requestId).catch(() => false);
        }
        if (closeTimerRef.current) {
          clearTimeout(closeTimerRef.current);
          closeTimerRef.current = null;
        }
        document.documentElement.setAttribute('data-theme', payload.theme);
        document.body.setAttribute('arco-theme', payload.theme);
        injectCompanionCustomCss(payload.customCss);
        const nextPhase = sameRequest ? phaseRef.current : 'preparing';
        snapshotRef.current = payload;
        setSnapshot(payload);
        setPhase(nextPhase);
        phaseRef.current = nextPhase;
        if (!sameRequest) activationPendingRef.current = false;
      }),
      listenCurrentWindow<MemoryPanelPresentPayload>(MEMORY_PANEL_EVENTS.present, (payload) => {
        if (snapshotRef.current?.requestId !== payload.requestId || phaseRef.current !== 'preparing') return;
        setPlacement(payload.placement);
        phaseRef.current = 'opening';
        setPhase('opening');
      }),
      listenCurrentWindow<MemoryPanelVisiblePayload>(MEMORY_PANEL_EVENTS.visible, (payload) => {
        if (snapshotRef.current?.requestId !== payload.requestId || phaseRef.current !== 'opening') return;
        setPhase('open');
        phaseRef.current = 'open';
        requestAnimationFrame(() => {
          void import('@tauri-apps/api/window').then(({ getCurrentWindow }) =>
            getCurrentWindow().isFocused().then((focused) => {
              if (!focused && !activationPendingRef.current) {
                void finishClose({ requestId: payload.requestId, reason: 'blur' });
                return;
              }
              if (focused) firstItemRef.current?.focus({ preventScroll: true });
            })
          );
        });
      }),
      listenCurrentWindow<MemoryPanelClosePayload>(MEMORY_PANEL_EVENTS.close, (payload) => void finishClose(payload)),
      listenCurrentWindow<MemoryPanelActionAckPayload>(MEMORY_PANEL_EVENTS.actionAck, (payload) => {
        if (snapshotRef.current?.requestId !== payload.requestId) return;
        activationPendingRef.current = false;
        if (payload.ok) {
          void finishClose({ requestId: payload.requestId, reason: 'activation' });
          return;
        }
        void import('@tauri-apps/api/window').then(({ getCurrentWindow }) =>
          getCurrentWindow().isFocused().then((focused) => {
            if (!focused && snapshotRef.current?.requestId === payload.requestId && phaseRef.current === 'open') {
              void finishClose({ requestId: payload.requestId, reason: 'blur' });
            }
          })
        );
      }),
    ]).then((items) => disposed ? items.forEach((unlisten) => unlisten()) : unlisteners.push(...items));

    let unlistenFocus: (() => void) | undefined;
    void import('@tauri-apps/api/window').then(({ getCurrentWindow }) => getCurrentWindow().onFocusChanged(({ payload: focused }) => {
      if (focused || phaseRef.current !== 'open' || activationPendingRef.current) return;
      const current = snapshotRef.current;
      if (current) void finishClose({ requestId: current.requestId, reason: 'blur' });
    })).then((unlisten) => { if (disposed) unlisten(); else unlistenFocus = unlisten; });
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
      unlistenFocus?.();
      if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    };
  }, [finishClose]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const current = snapshotRef.current;
      if (event.key === 'Escape' && current) void finishClose({ requestId: current.requestId, reason: 'escape' });
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [finishClose]);

  if (!snapshot) return null;
  const activate = (suggestionId: string) => {
    if (activationPendingRef.current) return;
    activationPendingRef.current = true;
    const payload: MemoryPanelActivatePayload = { requestId: snapshot.requestId, ownerWindowLabel: snapshot.ownerWindowLabel, suggestionId };
    void emitToWindow(snapshot.ownerWindowLabel, MEMORY_PANEL_EVENTS.activate, payload).catch(() => { activationPendingRef.current = false; });
  };

  return (
    <main className={`nomi-memory-panel nomi-memory-panel--${phase} nomi-memory-panel--${placement}`}>
      <section ref={cardRef} className='nomi-memory-panel__card' role='dialog' aria-label={t('nomi.tabs.suggestions')}>
        <div className='nomi-memory-panel__list'>
          {snapshot.suggestions.map((suggestion, index) => (
            <button ref={index === 0 ? firstItemRef : undefined} key={suggestion.id} type='button' className='nomi-memory-panel__item' onClick={() => activate(suggestion.id)}>
              <span className='nomi-memory-panel__title'>{suggestion.title}</span>
              <span className='nomi-memory-panel__body'>{suggestion.body}</span>
            </button>
          ))}
        </div>
      </section>
    </main>
  );
};

export default MemoryPanelPage;
