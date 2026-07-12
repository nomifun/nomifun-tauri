import React, { Suspense, useEffect } from 'react';
import { HashRouter, Navigate, Route, Routes, useLocation, useNavigate, useParams } from 'react-router-dom';
import AppLoader from '@renderer/components/layout/AppLoader';
import RouteErrorBoundary from '@renderer/components/layout/RouteErrorBoundary';
import { useAuth } from '@renderer/hooks/context/AuthContext';
import { useCompanionWindowsSync } from '@renderer/hooks/useCompanionWindowsSync';
import { useTrayLabels } from '@renderer/hooks/useTrayLabels';
import { isTauriRuntime } from '@/common/adapter/tauriRuntime';
const Conversation = React.lazy(() => import('@renderer/pages/conversation'));
const Guid = React.lazy(() => import('@renderer/pages/guid'));
const AssistantSettings = React.lazy(() => import('@renderer/pages/settings/AssistantSettings'));
const ModelHubPage = React.lazy(() => import('@renderer/pages/modelHub'));
const McpPage = React.lazy(() => import('@renderer/pages/mcp'));
const OpenCapabilitiesPage = React.lazy(() => import('@renderer/pages/openCapabilities'));
const SystemSettings = React.lazy(() => import('@renderer/pages/settings/SystemSettings'));
const ExtensionSettingsPage = React.lazy(() => import('@renderer/pages/settings/ExtensionSettingsPage'));
const LoginPage = React.lazy(() => import('@renderer/pages/login'));
const ComponentsShowcase = React.lazy(() => import('@renderer/pages/TestShowcase'));
const ScheduledTasksPage = React.lazy(() => import('@renderer/pages/cron/ScheduledTasksPage'));
const TaskDetailPage = React.lazy(() => import('@renderer/pages/cron/ScheduledTasksPage/TaskDetailPage'));
const RequirementsLayout = React.lazy(() => import('@renderer/pages/requirements/RequirementsLayout'));
const WorkspacePage = React.lazy(() => import('@renderer/pages/requirements/WorkspacePage'));
const ExtensionsPage = React.lazy(() => import('@renderer/pages/requirements/ExtensionsPage'));
const SourcesPage = React.lazy(() => import('@renderer/pages/requirements/SourcesPage'));
const TerminalSessionPage = React.lazy(() => import('@renderer/pages/terminal/TerminalSessionPage'));
const TerminalCreatePage = React.lazy(() => import('@renderer/pages/terminal/TerminalCreatePage'));
const NomiConfigPage = React.lazy(() => import('@renderer/pages/nomi'));
const PublicCompanionRosterPage = React.lazy(() => import('@renderer/pages/publicCompanion'));
const PublicAgentDetailPage = React.lazy(() => import('@renderer/pages/publicCompanion/PublicAgentDetailPage'));
const KnowledgeListPage = React.lazy(() => import('@renderer/pages/knowledge/KnowledgeListPage'));
const KnowledgeDetailPage = React.lazy(() => import('@renderer/pages/knowledge/KnowledgeDetailPage'));
const WorkshopListPage = React.lazy(() => import('@renderer/pages/workshop'));
const WorkshopCanvasPage = React.lazy(() => import('@renderer/pages/workshop/CanvasPage'));
const AssetLibraryPage = React.lazy(() => import('@renderer/pages/assets'));
const CompanionPage = React.lazy(() => import('@renderer/pages/companion'));
const MemoryPanelPage = React.lazy(() => import('@renderer/pages/memoryPanel'));
const ConversationShell = React.lazy(() => import('@renderer/pages/conversation/components/ConversationShell'));

const withRouteFallback = (Component: React.LazyExoticComponent<React.ComponentType>) => (
  <RouteErrorBoundary>
    <Suspense fallback={<AppLoader />}>
      <Component />
    </Suspense>
  </RouteErrorBoundary>
);

const withSearch = (path: string, searchParams: URLSearchParams) => {
  const search = searchParams.toString();
  return search ? `${path}?${search}` : path;
};

const LegacyExtensionsRedirect: React.FC = () => {
  const { search } = useLocation();
  const searchParams = new URLSearchParams(search);
  const tab = searchParams.get('tab');
  searchParams.delete('tab');

  if (tab === 'tools') {
    return <Navigate to={withSearch('/mcp', searchParams)} replace />;
  }

  searchParams.set('tab', 'skills');
  return <Navigate to={withSearch('/assistants', searchParams)} replace />;
};

// Legacy `/requirements/:id/edit` deep links → open the workspace with the
// requirement pre-selected in edit mode (the new shell hosts editing in a
// drawer, not a standalone form page).
const RequirementEditRedirect: React.FC = () => {
  const { id } = useParams();
  return <Navigate to={`/requirements?req=${id}&edit=1`} replace />;
};

const getHashRouteRedirectUrl = () => {
  if (typeof window === 'undefined') return null;
  if (!['http:', 'https:'].includes(window.location.protocol)) return null;
  if (window.location.hash) return null;

  const { origin, pathname, search } = window.location;
  if (pathname === '/' || pathname.endsWith('/index.html')) return null;

  return `${origin}/#${pathname}${search}`;
};

const ProtectedLayout: React.FC<{ layout: React.ReactElement }> = ({ layout }) => {
  const { status } = useAuth();

  if (status === 'checking') {
    return <AppLoader />;
  }

  if (status !== 'authenticated') {
    return <Navigate to='/login' replace />;
  }

  return (
    <>
      <CompanionNavigateListener />
      <CompanionWindowsSyncMount />
      <TrayLabelsMount />
      {React.cloneElement(layout)}
    </>
  );
};

// Owns the native desktop-companion window set from the main window: reconciles one
// `companion-{companion_id}` Tauri window per enabled companion (useCompanionWindowsSync). Inert
// outside the Tauri desktop shell.
const CompanionWindowsSyncMount: React.FC = () => {
  useCompanionWindowsSync();
  return null;
};

// Keeps the native system-tray menu labels (Show / Quit) in sync with the UI
// locale. Inert outside the Tauri desktop shell.
const TrayLabelsMount: React.FC = () => {
  useTrayLabels();
  return null;
};

// Listens for "companion-navigate" Tauri events emitted by the companion window (a click
// on a suggestion bubble / its context menu) and routes the main window.
// Inert outside the Tauri desktop shell.
const CompanionNavigateListener: React.FC = () => {
  const navigate = useNavigate();
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let unlisten: (() => void) | undefined;
    let disposed = false;
    void import('@tauri-apps/api/event').then(({ listen }) =>
      listen<string>('companion-navigate', (event) => {
        if (typeof event.payload === 'string' && event.payload.startsWith('/')) {
          void navigate(event.payload);
        }
      }).then((fn) => {
        if (disposed) fn();
        else unlisten = fn;
      })
    );
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [navigate]);
  return null;
};

const PanelRoute: React.FC<{ layout: React.ReactElement }> = ({ layout }) => {
  const { status } = useAuth();
  const hashRouteRedirectUrl = getHashRouteRedirectUrl();

  if (hashRouteRedirectUrl) {
    window.location.replace(hashRouteRedirectUrl);
    return <AppLoader />;
  }

  return (
    <HashRouter>
      <Routes>
        <Route
          path='/login'
          element={status === 'authenticated' ? <Navigate to='/guid' replace /> : withRouteFallback(LoginPage)}
        />
        {/* The desktop-companion window route: fullscreen transparent, no app layout/sidebar. */}
        <Route path='/companion' element={withRouteFallback(CompanionPage)} />
        <Route path='/nomi-memory-panel' element={withRouteFallback(MemoryPanelPage)} />
        <Route element={<ProtectedLayout layout={layout} />}>
          <Route index element={<Navigate to='/guid' replace />} />
          {/* Model Management, Assistant & Skill, and MCP — top-level homepage destinations */}
          <Route path='/models' element={withRouteFallback(ModelHubPage)} />
          <Route path='/extensions' element={<LegacyExtensionsRedirect />} />
          <Route path='/mcp' element={withRouteFallback(McpPage)} />
          <Route path='/open-capabilities' element={withRouteFallback(OpenCapabilitiesPage)} />
          {/* Assistants — relocated out of Settings into a top-level homepage destination */}
          <Route path='/assistants' element={withRouteFallback(AssistantSettings)} />
          {/* Session section — the secondary sidebar (ContentSider) persists across these routes */}
          <Route
            element={
              <Suspense fallback={<AppLoader />}>
                <ConversationShell />
              </Suspense>
            }
          >
            <Route path='/guid' element={withRouteFallback(Guid)} />
            <Route path='/conversation/:id' element={withRouteFallback(Conversation)} />
            <Route path='/terminal-new' element={withRouteFallback(TerminalCreatePage)} />
            <Route path='/terminal/:id' element={withRouteFallback(TerminalSessionPage)} />
          </Route>
          {/* Relocated to the homepage: Agents/Models → /models, Capabilities → Assistant & Skill / MCP */}
          <Route path='/settings/model' element={<Navigate to='/models?section=models' replace />} />
          <Route path='/settings/agent' element={<Navigate to='/models?section=agents' replace />} />
          <Route path='/settings/capabilities' element={<Navigate to='/assistants?tab=skills' replace />} />
          <Route path='/settings/skills-hub' element={<Navigate to='/assistants?tab=skills' replace />} />
          <Route path='/settings/tools' element={<Navigate to='/open-capabilities' replace />} />
          <Route path='/settings/display' element={<Navigate to='/settings/system' replace />} />
          <Route path='/settings/webui' element={<Navigate to='/open-capabilities' replace />} />
          <Route path='/settings/system' element={withRouteFallback(SystemSettings)} />
          <Route path='/settings/agent-runtime' element={withRouteFallback(SystemSettings)} />
          <Route path='/settings/browser-use' element={withRouteFallback(SystemSettings)} />
          <Route path='/settings/computer-use' element={withRouteFallback(SystemSettings)} />
          <Route path='/settings/about' element={withRouteFallback(SystemSettings)} />
          <Route path='/settings/ext/:tabId' element={withRouteFallback(ExtensionSettingsPage)} />
          {/* Relocated out of Settings → AI Core: Assistants → /assistants, Webhook → requirements 扩展能力 */}
          <Route path='/settings/assistants' element={<Navigate to='/assistants?tab=assistants' replace />} />
          <Route path='/settings/webhook' element={<Navigate to='/requirements/extensions?tab=notify' replace />} />
          <Route path='/settings' element={<Navigate to='/settings/system' replace />} />
          <Route path='/test/components' element={withRouteFallback(ComponentsShowcase)} />
          <Route path='/scheduled' element={withRouteFallback(ScheduledTasksPage)} />
          <Route path='/scheduled/:job_id' element={withRouteFallback(TaskDetailPage)} />
          {/* Requirements platform — nested shell (ContentSider persists across sections) */}
          <Route path='/requirements' element={withRouteFallback(RequirementsLayout)}>
            <Route index element={withRouteFallback(WorkspacePage)} />
            <Route path='extensions' element={withRouteFallback(ExtensionsPage)} />
            <Route path='sources' element={withRouteFallback(SourcesPage)} />
          </Route>
          {/* Legacy requirement routes → fold into the new shell (preserve deep links) */}
          <Route path='/requirements/kanban' element={<Navigate to='/requirements?view=board' replace />} />
          <Route path='/requirements/new' element={<Navigate to='/requirements?new=1' replace />} />
          <Route path='/requirements/:id/edit' element={<RequirementEditRedirect />} />
          <Route path='/requirements/tag-sessions' element={<Navigate to='/requirements/extensions?tab=autowork' replace />} />
          <Route path='/autowork' element={<Navigate to='/requirements/extensions?tab=autowork' replace />} />
          {/* Webhook config relocated into 扩展能力 */}
          <Route path='/other' element={<Navigate to='/requirements/extensions?tab=notify' replace />} />
          <Route path='/nomi' element={withRouteFallback(NomiConfigPage)} />
          {/* 对外伙伴 (Public Companion) — a first-class domain separate from desktop companions. */}
          <Route path='/public-companions' element={withRouteFallback(PublicCompanionRosterPage)} />
          <Route path='/public-companions/:id' element={withRouteFallback(PublicAgentDetailPage)} />
          <Route path='/knowledge' element={withRouteFallback(KnowledgeListPage)} />
          <Route path='/knowledge/:id' element={withRouteFallback(KnowledgeDetailPage)} />
          {/* 资产库 (Asset Library) — platform-level management of workshop assets. */}
          <Route path='/assets' element={withRouteFallback(AssetLibraryPage)} />
          {/* 创意工坊 (Creative Workshop) — infinite-canvas AI visual creation. */}
          <Route path='/workshop' element={withRouteFallback(WorkshopListPage)} />
          <Route path='/workshop/:id' element={withRouteFallback(WorkshopCanvasPage)} />
        </Route>
        <Route path='*' element={<Navigate to={status === 'authenticated' ? '/guid' : '/login'} replace />} />
      </Routes>
    </HashRouter>
  );
};

export default PanelRoute;
