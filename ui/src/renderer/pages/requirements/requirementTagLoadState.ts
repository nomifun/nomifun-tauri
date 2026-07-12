export interface RequirementTagSummary {
  tag: string;
  done: number;
  total: number;
}

export interface RequirementTagLoadState {
  tags: RequirementTagSummary[];
  loading: boolean;
  error: string | null;
  activeRequestId: number | null;
}

export type RequirementTagLoadAction =
  | { type: 'start'; requestId: number }
  | { type: 'success'; requestId: number; tags: RequirementTagSummary[] }
  | { type: 'failure'; requestId: number; error: string }
  | { type: 'finish'; requestId: number };

export const initialRequirementTagLoadState: RequirementTagLoadState = {
  tags: [],
  loading: true,
  error: null,
  activeRequestId: null,
};

export function reduceRequirementTagLoadState(
  state: RequirementTagLoadState,
  action: RequirementTagLoadAction
): RequirementTagLoadState {
  if (action.type !== 'start' && action.requestId !== state.activeRequestId) {
    return state;
  }

  switch (action.type) {
    case 'start':
      return { ...state, loading: true, activeRequestId: action.requestId };
    case 'success':
      return { ...state, tags: action.tags, error: null };
    case 'failure':
      return { ...state, error: action.error };
    case 'finish':
      return { ...state, loading: false };
  }
}
