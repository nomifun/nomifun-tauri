/**
 * Pure turn-state reducer for the nomi conversation hook.
 *
 * Replaces the ad-hoc imperative flips of three independent booleans
 * (`streamRunning` / `hasActiveTools` / `waitingResponse`) scattered across the
 * event handler — each with its own ref kept in manual lockstep — with a single
 * source of truth driven by a typed event stream (design §3.2: "the frontend
 * becomes a pure reducer over lifecycle events").
 *
 * Why this matters: the old code never cleared `hasActiveTools` on `finish` /
 * `error`, so a turn that ended while its last `tool_group` still reported an
 * active tool left `running` stuck true forever — a stuck-spinner ("卡死") bug.
 * Modelling the transitions explicitly fixes that by construction and makes the
 * behaviour exhaustively testable.
 */

export interface NomiTurnState {
  /** The model stream is active (text/thinking/tool output flowing). */
  streamRunning: boolean;
  /** One or more tools are executing / confirming / pending. */
  hasActiveTools: boolean;
  /** Between a tool batch finishing and the next model request — the backend
   * will send another turn, so the UI must keep showing activity. */
  waitingResponse: boolean;
}

export type NomiTurnEvent =
  /** New conversation / explicit stop: clear everything. */
  | { type: 'reset' }
  /** Async hydration from backend `is_processing`.
   *
   * The ordinary form is raise-only so a locally-raised submit survives a
   * stale query. `settleIdle` is reserved for the initial snapshot whose
   * lifecycle generation is still unchanged; in that case an idle backend is
   * authoritative and must clear stream activity delivered late from the
   * already-completed prior turn. */
  | { type: 'hydrate'; isRunning: boolean; settleIdle?: boolean }
  /** External setter (the send box raises this on submit). */
  | { type: 'setWaiting'; value: boolean }
  /** Any stream activity that should (re)assert the running state without
   * touching `waitingResponse`: start, thinking, permission prompt, or other
   * non-content output. Subsumes the old "auto-recover streamRunning" hacks. */
  | { type: 'activity' }
  /** Assistant text/content: running, and no longer waiting for the model.
   * A complete projection is rendered in the transcript without changing the
   * current turn because it is not part of a model stream. */
  | { type: 'content'; streamComplete?: boolean }
  /** A tool-group update carrying whether any tool is active and whether the
   * group is non-empty. */
  | { type: 'toolGroup'; hasActive: boolean; hasAny: boolean }
  /** Clean terminal event. */
  | { type: 'finish' }
  /** Error terminal event. */
  | { type: 'error' };

export const initialNomiTurnState: NomiTurnState = {
  streamRunning: false,
  hasActiveTools: false,
  waitingResponse: false,
};

/** Combined "is this turn doing anything" flag the UI spins on. */
export function isTurnRunning(s: NomiTurnState): boolean {
  return s.waitingResponse || s.streamRunning || s.hasActiveTools;
}

export function nomiTurnReducer(state: NomiTurnState, event: NomiTurnEvent): NomiTurnState {
  switch (event.type) {
    case 'reset':
      return { ...initialNomiTurnState };

    case 'hydrate':
      if (!event.isRunning && event.settleIdle) {
        return { ...initialNomiTurnState };
      }
      // Raise-only: OR the backend's running signal onto whatever is already
      // locally raised; tools are re-derived from incoming messages, so clear.
      return {
        streamRunning: event.isRunning || state.streamRunning,
        waitingResponse: event.isRunning || state.waitingResponse,
        hasActiveTools: false,
      };

    case 'setWaiting':
      return { ...state, waitingResponse: event.value };

    case 'activity':
      // Auto-recover: any activity asserts the stream is running again (handles
      // events that arrive after a premature finish) without altering waiting.
      return { ...state, streamRunning: true };

    case 'content':
      if (event.streamComplete) {
        return state;
      }
      return { ...state, streamRunning: true, waitingResponse: false };

    case 'toolGroup': {
      // A tool batch transitioning from active → inactive means the backend will
      // issue another model request, so raise `waitingResponse`.
      const finishedToolBatch = state.hasActiveTools && !event.hasActive && event.hasAny;
      return {
        streamRunning: true,
        hasActiveTools: event.hasActive,
        waitingResponse: finishedToolBatch ? true : state.waitingResponse,
      };
    }

    case 'finish':
    case 'error':
      // Terminal: the turn is over. Clear ALL activity — notably hasActiveTools,
      // which the old code left set and caused a permanently stuck spinner.
      return { ...initialNomiTurnState };

    default:
      return state;
  }
}
