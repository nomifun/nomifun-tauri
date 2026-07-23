/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */
import type { ConversationId } from '@/common/types/ids';

import { ipcBridge } from '@/common';
import type { IIdmmConfig, IKnowledgeBinding } from '@/common/adapter/ipcBridge';
import type { TChatConversation } from '@/common/config/storage';
import { Message } from '@arco-design/web-react';
import { useCallback, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { AutoWorkDraftValue } from '@/renderer/pages/conversation/components/AutoWorkControl';
import { defaultIdmmConfig } from '@/renderer/pages/conversation/components/IdmmControl';
import { defaultKnowledgeBinding } from '@/renderer/pages/conversation/components/KnowledgeControl';
import { workpathKeyForConversation } from '@/renderer/pages/conversation/SessionList/utils/sessionWorkpath';

export type GuidAdvancedConfig = {
  knowledge: IKnowledgeBinding;
  setKnowledge: (next: IKnowledgeBinding) => void;
  autoWork: AutoWorkDraftValue;
  setAutoWork: (next: AutoWorkDraftValue) => void;
  idmm: IIdmmConfig;
  setIdmm: (next: IIdmmConfig) => void;
  /** Push the enabled drafts onto a freshly created conversation. Never
   * throws — a feature that fails to apply degrades to a warning toast so
   * the navigation into the conversation is not blocked. */
  applyToConversation: (conversationId: ConversationId) => Promise<void>;
  reset: () => void;
};

/**
 * Draft state for the Guid page's advanced per-conversation features
 * (knowledge mounts / AutoWork / IDMM). These are stored outside
 * the conversation-create payload — each has its own endpoint keyed by
 * conversation id — so the Guid page collects them up front and applies
 * them right after `conversation.create` returns, before navigating.
 */
export const useGuidAdvancedConfig = (): GuidAdvancedConfig => {
  const { t } = useTranslation();
  const [knowledge, setKnowledge] = useState<IKnowledgeBinding>(defaultKnowledgeBinding);
  const [autoWork, setAutoWork] = useState<AutoWorkDraftValue>({ enabled: false });
  const [idmm, setIdmm] = useState<IIdmmConfig>(defaultIdmmConfig);

  // The apply call runs inside useGuidSend's stable callback chain — read the
  // latest drafts through a ref so send handlers never capture stale state.
  const draftsRef = useRef({ knowledge, autoWork, idmm });
  draftsRef.current = { knowledge, autoWork, idmm };

  const applyToConversation = useCallback(
    async (conversationId: ConversationId) => {
      const { knowledge: kb, autoWork: aw, idmm: idm } = draftsRef.current;
      const tasks: Array<{ label: string; run: () => Promise<unknown> }> = [];

      // Persist any non-default binding (not just enabled ones) so the
      // conversation header shows the same picks the user made here — e.g.
      // pre-selected bases or writeback without the master switch.
      const kbTouched =
        kb.enabled ||
        kb.writeback ||
        kb.kb_ids.length > 0 ||
        kb.writeback_mode !== 'staged' ||
        kb.writeback_eagerness !== 'conservative';
      if (kbTouched) {
        tasks.push({
          label: t('knowledge.control.label'),
          // Knowledge binds at workpath scope now (spec §7) — resolve the
          // just-created conversation's workpath so the header KnowledgeControl
          // (which also reads by workpath) reads these picks back instead of a
          // dangling conversation-scoped row.
          run: async () => {
            const conv = (await ipcBridge.conversation.get.invoke({ conversation_id: conversationId })) as TChatConversation | undefined;
            const workpath = workpathKeyForConversation(conv?.extra as Record<string, unknown> | undefined);
            return ipcBridge.knowledge.setBinding.invoke({ kind: 'workpath', target_id: workpath, ...kb });
          },
        });
      }
      if (idm.fault_watch.enabled || idm.decision_watch.enabled) {
        // The backend resolves the backup model from the conversation's own
        // model when a model-tier watch leaves bypass_model empty — so just
        // apply and let the backend surface any genuine 400 via the warning
        // below.
        tasks.push({
          label: t('idmm.label'),
          run: () => ipcBridge.idmm.set.invoke({ kind: 'conversation', target_id: conversationId, ...idm }),
        });
      }

      const report = (tasks_: Array<{ label: string }>, results: PromiseSettledResult<unknown>[]) => {
        results.forEach((result, i) => {
          if (result.status === 'rejected') {
            console.error(`[GuidAdvancedConfig] Failed to apply ${tasks_[i].label}:`, result.reason);
            Message.warning(t('guid.advanced.applyFailed', { feature: tasks_[i].label }));
          }
        });
      };

      if (tasks.length > 0) {
        report(tasks, await Promise.allSettled(tasks.map((task) => task.run())));
      }

      // AutoWork last, sequenced after knowledge/IDMM have settled: enabling
      // it starts the backend loop, which may pick up the first requirement
      // immediately — that first task should already see the mounts/IDMM.
      if (aw.enabled && aw.tag) {
        const awTask = {
          label: t('requirements.autowork.label'),
          run: () =>
            ipcBridge.requirements.setAutoWork.invoke({
              kind: 'conversation',
              target_id: conversationId,
              enabled: true,
              tag: aw.tag,
            }),
        };
        report([awTask], await Promise.allSettled([awTask.run()]));
      }
    },
    [t]
  );

  const reset = useCallback(() => {
    setKnowledge(defaultKnowledgeBinding());
    setAutoWork({ enabled: false });
    setIdmm(defaultIdmmConfig());
  }, []);

  return {
    knowledge,
    setKnowledge,
    autoWork,
    setAutoWork,
    idmm,
    setIdmm,
    applyToConversation,
    reset,
  };
};
