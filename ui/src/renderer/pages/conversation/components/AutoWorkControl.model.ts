export type AutoWorkTagPickerMode = 'loading' | 'error' | 'empty' | 'ready';

export function getAutoWorkTagPickerMode(
  tagCount: number,
  loading: boolean,
  error: string | null
): AutoWorkTagPickerMode {
  if (loading) return 'loading';
  if (tagCount > 0) return 'ready';
  if (error) return 'error';
  return 'empty';
}

export function isAutoWorkEnableBlocked(enabled: boolean, mode: AutoWorkTagPickerMode): boolean {
  return !enabled && mode !== 'ready';
}

export function shouldFocusAutoWorkTagPickerAction(
  mode: AutoWorkTagPickerMode,
  key: string,
  shiftKey: boolean
): boolean {
  return key === 'Tab' && !shiftKey && (mode === 'empty' || mode === 'error');
}

export function isAutoWorkTagPickerActionKey(key: string): boolean {
  return key === 'Enter' || key === ' ';
}
