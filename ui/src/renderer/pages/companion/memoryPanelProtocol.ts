import type { ICompanionSuggestion } from '@/common/adapter/ipcBridge';
import type { DetachedMemoryPanelPlacement } from './detachedMemoryPanelGeometry';

export const MEMORY_PANEL_LABEL = 'nomi-memory-panel';
export const MEMORY_PANEL_EVENTS = {
  probe: 'nomi-memory-panel://probe', ready: 'nomi-memory-panel://ready', snapshot: 'nomi-memory-panel://snapshot',
  measured: 'nomi-memory-panel://measured', present: 'nomi-memory-panel://present', visible: 'nomi-memory-panel://visible',
  close: 'nomi-memory-panel://close', closed: 'nomi-memory-panel://closed', activate: 'nomi-memory-panel://activate', actionAck: 'nomi-memory-panel://action-ack',
} as const;

export type MemoryPanelPhase = 'closed' | 'preparing' | 'opening' | 'open' | 'closing';
export type MemoryPanelCloseReason = 'blur' | 'escape' | 'toggle' | 'empty' | 'owner-invalid' | 'activation';
export type MemoryPanelToggleIntent = 'open' | 'close';
export interface MemoryPanelState { phase: MemoryPanelPhase; requestId: string | null; ownerCompanionId: string | null; closeReason: MemoryPanelCloseReason | null }
export const initialMemoryPanelState: MemoryPanelState = { phase: 'closed', requestId: null, ownerCompanionId: null, closeReason: null };
export const memoryPanelToggleIntent = (phase: MemoryPanelPhase): MemoryPanelToggleIntent =>
  phase === 'closed' || phase === 'closing' ? 'open' : 'close';
export const shouldCloseMemoryPanelForOwnerGeometryChange = (phase: MemoryPanelPhase): boolean =>
  phase === 'preparing' || phase === 'opening' || phase === 'open';
export type MemoryPanelAction =
  | { type: 'begin'; requestId: string; ownerCompanionId: string }
  | { type: 'opening' | 'opened' | 'closed'; requestId: string }
  | { type: 'request-close'; requestId: string; reason: MemoryPanelCloseReason };

export function memoryPanelReducer(state: MemoryPanelState, action: MemoryPanelAction): MemoryPanelState {
  if (action.type === 'begin') return { phase: 'preparing', requestId: action.requestId, ownerCompanionId: action.ownerCompanionId, closeReason: null };
  if (state.requestId !== action.requestId) return state;
  if (action.type === 'opening' && state.phase === 'preparing') return { ...state, phase: 'opening' };
  if (action.type === 'opened' && (state.phase === 'preparing' || state.phase === 'opening')) return { ...state, phase: 'open' };
  if (action.type === 'request-close') {
    if (state.phase !== 'open') return state;
    return { ...state, phase: 'closing', closeReason: action.reason };
  }
  if (action.type === 'closed') return initialMemoryPanelState;
  return state;
}

let requestSequence = 0;
export const nextMemoryPanelRequestId = (ownerCompanionId: string) => `${ownerCompanionId}:${Date.now()}:${++requestSequence}`;

export interface MemoryPanelProbePayload { requestId: string; ownerWindowLabel: string }
export interface MemoryPanelReadyPayload extends MemoryPanelProbePayload {}
export interface MemoryPanelSnapshotPayload { requestId: string; ownerCompanionId: string; ownerWindowLabel: string; suggestions: ICompanionSuggestion[]; theme: 'light' | 'dark'; customCss: string }
export interface MemoryPanelMeasuredPayload { requestId: string; ownerWindowLabel: string; width: number; height: number }
export interface MemoryPanelPresentPayload { requestId: string; placement: DetachedMemoryPanelPlacement }
export interface MemoryPanelVisiblePayload { requestId: string }
export interface MemoryPanelClosePayload { requestId: string; reason: MemoryPanelCloseReason }
export interface MemoryPanelClosedPayload extends MemoryPanelClosePayload { restoreFocus: boolean }
export interface MemoryPanelActivatePayload { requestId: string; ownerWindowLabel: string; suggestionId: string }
export interface MemoryPanelActionAckPayload { requestId: string; suggestionId: string; ok: boolean }
