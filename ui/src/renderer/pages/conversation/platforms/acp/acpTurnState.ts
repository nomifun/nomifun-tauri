export type AcpTurnPhase =
  | 'idle'
  | 'waiting_first_output'
  | 'starting'
  | 'thinking'
  | 'streaming'
  | 'tooling'
  | 'waiting_permission'
  | 'error';

export interface AcpTurnState {
  phase: AcpTurnPhase;
  turnId?: string;
  processingStartedAt?: number;
}

export type AcpTurnEvent =
  | { type: 'reset' }
  | { type: 'submit'; turnId?: string; startedAt?: number }
  | { type: 'hydrate'; isRunning: boolean; processingStartedAt?: number }
  | { type: 'turnStarted'; turnId?: string; processingStartedAt?: number }
  | { type: 'activity' }
  | { type: 'thinking' }
  | { type: 'content' }
  | { type: 'tooling' }
  | { type: 'permission' }
  | { type: 'finish' }
  | { type: 'error' };

export const initialAcpTurnState: AcpTurnState = {
  phase: 'idle',
};

const busyPhases = new Set<AcpTurnPhase>([
  'waiting_first_output',
  'starting',
  'thinking',
  'streaming',
  'tooling',
  'waiting_permission',
]);

export function isAcpTurnBusy(state: AcpTurnState): boolean {
  return busyPhases.has(state.phase);
}

function withStartedAt(state: AcpTurnState, processingStartedAt?: number): Pick<AcpTurnState, 'processingStartedAt'> {
  return {
    processingStartedAt: processingStartedAt ?? state.processingStartedAt ?? Date.now(),
  };
}

export function acpTurnReducer(state: AcpTurnState, event: AcpTurnEvent): AcpTurnState {
  switch (event.type) {
    case 'reset':
    case 'finish':
      return { ...initialAcpTurnState };

    case 'submit':
      return {
        phase: 'waiting_first_output',
        turnId: event.turnId ?? state.turnId,
        processingStartedAt: event.startedAt ?? state.processingStartedAt ?? Date.now(),
      };

    case 'hydrate':
      if (!event.isRunning) {
        return isAcpTurnBusy(state) ? state : { ...initialAcpTurnState };
      }
      return {
        phase: state.phase === 'idle' || state.phase === 'error' ? 'starting' : state.phase,
        turnId: state.turnId,
        ...withStartedAt(state, event.processingStartedAt),
      };

    case 'turnStarted':
      return {
        phase: state.phase === 'thinking' || state.phase === 'streaming' ? state.phase : 'starting',
        turnId: event.turnId ?? state.turnId,
        ...withStartedAt(state, event.processingStartedAt),
      };

    case 'activity':
      return {
        phase: state.phase === 'idle' || state.phase === 'error' ? 'starting' : state.phase,
        turnId: state.turnId,
        ...withStartedAt(state),
      };

    case 'thinking':
      return {
        phase: 'thinking',
        turnId: state.turnId,
        ...withStartedAt(state),
      };

    case 'content':
      return {
        phase: 'streaming',
        turnId: state.turnId,
        ...withStartedAt(state),
      };

    case 'tooling':
      return {
        phase: 'tooling',
        turnId: state.turnId,
        ...withStartedAt(state),
      };

    case 'permission':
      return {
        phase: 'waiting_permission',
        turnId: state.turnId,
        ...withStartedAt(state),
      };

    case 'error':
      return {
        phase: 'error',
      };

    default:
      return state;
  }
}
