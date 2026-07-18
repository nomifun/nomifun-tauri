import { ipcBridge } from '@/common';
import { Message } from '@arco-design/web-react';
import React, { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate, useParams } from 'react-router-dom';
import useSWR from 'swr';
import ChatConversation from './components/ChatConversation';
import MessageListSkeleton from './Messages/components/MessageListSkeleton';
import { useAutoTitle } from '@/renderer/hooks/chat/useAutoTitle';
import { getConversationOrNull } from '@/renderer/pages/conversation/utils/conversationCache';
import { parseConversationId } from '@/common/types/ids';
import { emitter } from '@/renderer/utils/emitter';

const ChatConversationIndex: React.FC = () => {
  const { id } = useParams();
  // Validate the route string once at the boundary; every downstream layer
  // keeps the same canonical conversation entity ID.
  const conversationId = id != null ? parseConversationId(id) : undefined;
  const { t } = useTranslation();
  const navigate = useNavigate();
  const { syncTitleFromHistory } = useAutoTitle();
  const notFoundHandledIdRef = useRef<string | undefined>(undefined);
  const deletedHandledIdRef = useRef<string | undefined>(undefined);
  const defaultConversationTitle = t('conversation.welcome.newConversation');

  const { data, isLoading, mutate } = useSWR(id ? `conversation/${id}` : null, () => {
    return getConversationOrNull(conversationId!);
  });

  useEffect(() => {
    if (!id) return;

    return ipcBridge.conversation.listChanged.on((event) => {
      if (event.conversation_id !== conversationId) {
        return;
      }

      if (event.action === 'deleted') {
        if (deletedHandledIdRef.current === id) {
          return;
        }
        deletedHandledIdRef.current = id;

        // The backend publishes `deleted` only after the persistent row is
        // authoritatively gone. This is also the success path for a delete
        // request whose HTTP waiter timed out while its detached coordinator
        // kept working. Reuse the local deletion channel so command queues and
        // other per-conversation state are cleared before the route unmounts.
        emitter.emit('conversation.deleted', conversationId);
        void mutate(undefined, { revalidate: false });
        void navigate('/guid', { replace: true });
        return;
      }

      if (event.action !== 'updated' && event.action !== 'created') {
        return;
      }

      void mutate();
    });
  }, [id, conversationId, mutate, navigate]);

  useEffect(() => {
    if (!data || data.name !== defaultConversationTitle) {
      return;
    }

    void syncTitleFromHistory(data.id);
  }, [data, defaultConversationTitle, syncTitleFromHistory]);

  // 会话不存在（例如从历史栈回到已删除会话）时，提示并替换路由到首页，
  // 避免渲染空骨架。每个 id 只触发一次。
  // Conversation does not exist (e.g. navigating back to a deleted one via
  // browser history): show a toast and replace the route with home, so we
  // don't render an empty skeleton. Fire at most once per id.
  useEffect(() => {
    if (!id || isLoading || data || notFoundHandledIdRef.current === id) return;
    notFoundHandledIdRef.current = id;
    Message.warning(t('conversation.notFound'));
    navigate('/', { replace: true });
  }, [id, isLoading, data, navigate, t]);

  if (isLoading) return <MessageListSkeleton />;
  return <ChatConversation conversation={data ?? undefined}></ChatConversation>;
};

export default ChatConversationIndex;
