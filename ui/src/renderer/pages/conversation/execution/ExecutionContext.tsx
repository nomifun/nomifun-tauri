import type { ConversationId, ExecutionId, ExecutionStepId } from '@/common/types/ids';
import type { TChatConversation } from '@/common/config/storage';
import type { TAgentExecutionDetail } from '@/common/types/agentExecution/agentExecutionTypes';
import React, { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import type { OpenStepPayload } from './DagCanvas';
import type { LeadThinkingState } from './useLeadThinking';
import { useConversationExecution } from './useConversationExecution';

export interface ExecutionContextValue {
  conversationId: ConversationId;
  executionId: ExecutionId | null;
  detail: TAgentExecutionDetail | null;
  refetch: () => Promise<void>;
  leadThinking: LeadThinkingState;
  loading: boolean;
  projectedStepId: ExecutionStepId | null;
  projectedPayload: OpenStepPayload | null;
  projectStep: (payload: OpenStepPayload) => void;
  returnToMain: () => void;
  canvasOpen: boolean;
  setCanvasOpen: (open: boolean) => void;
  toggleCanvas: () => void;
}

const ExecutionContext = createContext<ExecutionContextValue | null>(null);

export const ExecutionProvider: React.FC<{
  conversation: TChatConversation;
  children: React.ReactNode;
}> = ({ conversation, children }) => {
  const { executionId, detail, refetch, leadThinking, loading } = useConversationExecution(conversation);
  const [projectedPayload, setProjectedPayload] = useState<OpenStepPayload | null>(null);
  const [canvasOpen, setCanvasOpen] = useState(false);

  const projectStep = useCallback((payload: OpenStepPayload) => setProjectedPayload(payload), []);
  const returnToMain = useCallback(() => setProjectedPayload(null), []);
  const toggleCanvas = useCallback(() => setCanvasOpen((open) => !open), []);

  useEffect(() => {
    setProjectedPayload(null);
    setCanvasOpen(Boolean(executionId));
  }, [executionId]);

  const value = useMemo<ExecutionContextValue>(
    () => ({
      conversationId: conversation.id,
      executionId,
      detail,
      refetch,
      leadThinking,
      loading,
      projectedStepId: projectedPayload?.step.step_id ?? null,
      projectedPayload,
      projectStep,
      returnToMain,
      canvasOpen,
      setCanvasOpen,
      toggleCanvas,
    }),
    [
      canvasOpen,
      conversation.id,
      detail,
      executionId,
      leadThinking,
      loading,
      projectStep,
      projectedPayload,
      refetch,
      returnToMain,
      toggleCanvas,
    ],
  );

  return <ExecutionContext.Provider value={value}>{children}</ExecutionContext.Provider>;
};

export function useExecution(): ExecutionContextValue {
  const context = useContext(ExecutionContext);
  if (!context) throw new Error('useExecution must be used within an <ExecutionProvider>');
  return context;
}

export function useExecutionSafe(): ExecutionContextValue | null {
  return useContext(ExecutionContext);
}
