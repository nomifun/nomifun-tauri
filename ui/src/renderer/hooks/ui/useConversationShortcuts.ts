import type { ConversationId } from '@/common/types/ids';
import { useEffect } from 'react';
import type { NavigateFunction } from 'react-router-dom';
import { useLocation } from 'react-router-dom';
import { useVisibleConversationIds } from '@/renderer/pages/conversation/SessionList/hooks/useVisibleConversationIds';
import { isDesktopShell } from '@/renderer/utils/platform';
import { parseSessionRoute } from '@/renderer/utils/routes/sessionRoute';

type UseConversationShortcutsParams = {
  navigate: NavigateFunction;
};

const getCycledConversationId = (
  visibleConversationIds: ConversationId[],
  activeConversationId: ConversationId | null,
  direction: 1 | -1
): ConversationId | null => {
  if (visibleConversationIds.length < 2 || activeConversationId == null) {
    return null;
  }

  const activeIndex = visibleConversationIds.findIndex((conversation_id) => conversation_id === activeConversationId);
  if (activeIndex === -1) {
    return null;
  }

  const nextIndex = (activeIndex + direction + visibleConversationIds.length) % visibleConversationIds.length;
  return visibleConversationIds[nextIndex] ?? null;
};

const isConversationTabShortcut = (event: KeyboardEvent): boolean => {
  return event.ctrlKey && !event.metaKey && !event.altKey && event.key === 'Tab';
};

const isNewConversationShortcut = (event: KeyboardEvent): boolean => {
  return (event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 't';
};

export const useConversationShortcuts = ({ navigate }: UseConversationShortcutsParams): void => {
  const location = useLocation();
  const visibleConversationIds = useVisibleConversationIds();

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) {
        return;
      }

      if (!isDesktopShell()) {
        return;
      }

      if (isConversationTabShortcut(event)) {
        event.preventDefault();
        const activeSession = parseSessionRoute(location.pathname);
        const currentConversationId = activeSession?.kind === 'conversation' ? activeSession.id : null;
        const targetConversationId = getCycledConversationId(
          visibleConversationIds,
          currentConversationId,
          event.shiftKey ? -1 : 1
        );

        if (targetConversationId != null) {
          void navigate(`/conversation/${targetConversationId}`);
        }
        return;
      }

      if (isNewConversationShortcut(event)) {
        event.preventDefault();
        void navigate('/guid');
      }
    };

    window.addEventListener('keydown', handleKeyDown);
    return () => {
      window.removeEventListener('keydown', handleKeyDown);
    };
  }, [location.pathname, navigate, visibleConversationIds]);
};
