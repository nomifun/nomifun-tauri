/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useEffect } from 'react';
import { useOrchestrationSafe } from './OrchestrationContext';
import ProjectedWorkerView from './ProjectedWorkerView';

type ConversationContentSwitcherProps = {
  /** The main agent content (= NomiChat). ALWAYS mounted; only its visibility is
   * toggled while a worker node is projected. */
  children: React.ReactNode;
};

/**
 * ConversationContentSwitcher — the content-area projection switch for
 * 「会话原生编排 v2」(F7). Sits between {@link ChatLayout}'s content slot and the
 * main {@link NomiChat}, projecting a clicked DAG worker node's read-only
 * transcript over the (still-mounted) main conversation and back.
 *
 * Invariants:
 *  - **The main agent content (children = NomiChat) is ALWAYS mounted.** When a
 *    node is projected we hide it with `display:none` rather than unmounting it,
 *    so the user's input draft + scroll position survive the round-trip and the
 *    expensive NomiChat subtree never tears down / re-initializes. Returning to
 *    main is a pure visibility flip.
 *  - **Default = main.** `projectedTaskId === null` (the F3 default, and reset on
 *    every run change / returnToMain) → only the children are visible, no overlay.
 *  - **No provider → passthrough.** Surfaces without an
 *    {@link OrchestrationProvider} (e.g. companion chat) read `null` from
 *    {@link useOrchestrationSafe} and just render the children verbatim — zero
 *    projection, no crash.
 *
 * The projected view is absolutely positioned over the content region (the root
 * is `relative`), so it overlays the hidden main content within the same flex
 * box and inherits its sizing.
 */
const ConversationContentSwitcher: React.FC<ConversationContentSwitcherProps> = ({ children }) => {
  const orchestration = useOrchestrationSafe();

  // A stale projectedTaskId without a cached payload (shouldn't happen — F3 sets
  // them together) would render an empty overlay over a hidden conversation.
  // Reconcile back to main from an effect (never mutate state during render).
  const staleProjection =
    orchestration !== null && orchestration.projectedTaskId !== null && orchestration.projectedPayload === null;
  const returnToMain = orchestration?.returnToMain;
  useEffect(() => {
    if (staleProjection) returnToMain?.();
  }, [staleProjection, returnToMain]);

  // No provider (companion chat, etc.) → render the main content verbatim. No
  // wrapper, no projection — keeps non-orchestration surfaces byte-identical.
  if (!orchestration) {
    return <>{children}</>;
  }

  const { projectedTaskId, projectedPayload } = orchestration;
  // Project only when we have BOTH the id and the cached payload to resolve the
  // worker conversation from.
  const projecting = projectedTaskId !== null && projectedPayload !== null;

  return (
    <div className='relative flex flex-1 flex-col min-h-0'>
      {/* Main agent content — ALWAYS mounted; only hidden while projecting so
          scroll + input draft are preserved (never conditionally unmounted). */}
      <div
        className='flex flex-1 flex-col min-h-0'
        style={projecting ? { display: 'none' } : undefined}
        aria-hidden={projecting}
      >
        {children}
      </div>

      {/* Projected worker node — overlays the hidden main content. `key` by the
          projected task id so switching between nodes REMOUNTS the view (and its
          NodePreconfigPanel / collapse state), matching the repo's
          `key={conversation.id}` convention — otherwise an unsaved model/preset
          from the previous node would leak into the next node's form. */}
      {projecting && projectedPayload && (
        <ProjectedWorkerView key={projectedPayload.task.id} payload={projectedPayload} />
      )}
    </div>
  );
};

export default ConversationContentSwitcher;
