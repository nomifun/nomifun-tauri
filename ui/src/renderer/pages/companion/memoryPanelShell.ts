import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
import type { GeomRect } from './windowGeometry';
import { MEMORY_PANEL_LABEL } from './memoryPanelProtocol';

const invoke = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
  const core = await import('@tauri-apps/api/core');
  return core.invoke<T>(command, args);
};

export async function prepareMemoryPanelWindow(): Promise<void> {
  if (isTauriRuntime()) await invoke('prepare_companion_memory_panel');
}
export async function placeMemoryPanelWindow(args: { requestId: string; ownerCompanionId: string; rect: GeomRect }): Promise<void> {
  if (isTauriRuntime()) await invoke('place_companion_memory_panel', args);
}
export async function showMemoryPanelWindow(args: { requestId: string; ownerCompanionId: string }): Promise<boolean> {
  return isTauriRuntime() ? invoke<boolean>('show_companion_memory_panel', args) : false;
}
export async function hideMemoryPanelWindow(requestId: string): Promise<boolean> {
  return isTauriRuntime() ? invoke<boolean>('hide_companion_memory_panel', { requestId }) : false;
}
export async function emitToMemoryPanel<T>(event: string, payload: T): Promise<void> {
  if (!isTauriRuntime()) return;
  const { emitTo } = await import('@tauri-apps/api/event');
  await emitTo(MEMORY_PANEL_LABEL, event, payload);
}
export async function emitToWindow<T>(label: string, event: string, payload: T): Promise<void> {
  if (!isTauriRuntime()) return;
  const { emitTo } = await import('@tauri-apps/api/event');
  await emitTo(label, event, payload);
}
export async function listenCurrentWindow<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
  if (!isTauriRuntime()) return () => {};
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  return getCurrentWindow().listen<T>(event, ({ payload }) => handler(payload));
}
