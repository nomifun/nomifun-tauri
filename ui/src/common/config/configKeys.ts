import type { AcpInitializeResult, AcpSessionConfigOption, AcpSessionModes } from '@/common/types/platform/acpTypes';
import type { SpeechToTextConfig } from '@/common/types/provider/speech';
import type { ICssTheme, IMcpServer, TProviderWithModel } from '@/common/config/storage';

export type ConfigKeyMap = {
  'google.config': {
    proxy?: string;
  };
  'codex.config':
    | { cli_path?: string; yoloMode?: boolean; sandboxMode?: 'read-only' | 'workspace-write' | 'danger-full-access' }
    | undefined;
  'acp.config': {
    [backend: string]: {
      auth_methodId?: string;
      authToken?: string;
      lastAuthTime?: number;
      cli_path?: string;
      yoloMode?: boolean;
      preferredMode?: string;
      preferredModelId?: string;
      promptTimeout?: number;
    };
  };
  'acp.promptTimeout': number | undefined;
  'acp.agentIdleTimeout': number | undefined;
  'acp.cachedInitializeResult': Record<string, AcpInitializeResult> | undefined;
  'acp.cached_config_options': Record<string, AcpSessionConfigOption[]> | undefined;
  'acp.cachedModes': Record<string, AcpSessionModes> | undefined;
  'mcp.config': IMcpServer[];
  language: string;
  theme: string;
  colorScheme: string;
  'ui.zoomFactor': number | undefined;
  'window.bounds': { x?: number; y?: number; width: number; height: number } | undefined;
  'webui.desktop.enabled': boolean | undefined;
  'webui.desktop.allowRemote': boolean | undefined;
  'webui.desktop.port': number | undefined;
  customCss: string;
  'css.themes': ICssTheme[];
  'css.activeThemeId': string;
  'nomi.config': { preferredMode?: string } | undefined;
  'nomi.defaultModel': { id: string; use_model: string } | undefined;
  /**
   * 会话入口模型选择器三态：单一模型 / 自动编排 / 范围多选。
   * Guid-entry model selector tri-state: single model / auto-orchestrate / range multi-select.
   */
  'nomi.modelSelectionMode': 'single' | 'auto' | 'range' | undefined;
  // Default provider+model for the knowledge-base AI description/overview
  // generators (autogen / description.generate / description.polish). Empty
  // value = let the backend fall back to its own default completer model.
  'knowledge.autogenModel': { provider_id: string; model: string } | undefined;
  'tools.imageGenerationModel': TProviderWithModel & { switch?: boolean };
  'tools.speechToText': SpeechToTextConfig | undefined;
  'workspace.pasteConfirm': boolean | undefined;
  'upload.saveToWorkspace': boolean | undefined;
  'guid.lastSelectedAgent': string | undefined;
  'system.notificationEnabled': boolean | undefined;
  'system.cronNotificationEnabled': boolean | undefined;
  'system.keepAwake': boolean | undefined;
  'system.autoPreviewOfficeFiles': boolean | undefined;
  // Desktop control (computer-use): gates the nomi engine's Computer tool
  // (observe/click/type/launch). Read by the backend agent factory per session.
  'agent.computerUse': boolean | undefined;
  // Browser control (browser-use): gates the nomi engine's built-in browser
  // tools (native CDP engine). Off by default; enabling it fetches Chrome on
  // first use. Read by the backend agent factory per session.
  'agent.browserUse': boolean | undefined;
  // Persistent login (browser-use sub-setting): keeps cookies/storage across
  // sessions in an encrypted vault. ON by default. When on, evaluate full-power
  // mode is blocked (security mutex). Read by the backend browser engine.
  'agent.browserUse.persistentLogin': boolean | undefined;
  // Full-power browser evaluate mode: unlocks arbitrary page-script evaluation.
  // OFF by default and mutually exclusive with persistent login on the backend.
  'agent.browserUse.fullPower': boolean | undefined;
  // Site memory (browser-use sub-setting): persists per-site interaction hints to
  // disk + injects them into the agent's context. OFF by default (opt-in,
  // privacy-relevant). Read by the backend browser factory.
  'agent.browserUse.siteMemory': boolean | undefined;
  // Human takeover / approval (browser-use sub-setting): irreversible browser
  // actions + gated cross-origin POSTs are held for the user's approval instead of
  // hard-blocked. OFF by default (opt-in). Read by the backend agent factory.
  'agent.browserUse.takeover': boolean | undefined;
  // Visual fallback (browser-use sub-setting): when DOM/aria anchoring fails, the
  // agent screenshots the page and asks the vision model to locate the target, then
  // clicks the mapped point. OFF by default (opt-in, vision-token cost). Read by the
  // backend agent factory.
  'agent.browserUse.visualFallback': boolean | undefined;
  'assistant.telegram.agent':
    | { agent_type: string; backend?: string; id?: string; custom_agent_id?: string; name?: string }
    | undefined;
  // Master-agent greeter companion per IM platform (mirror of the backend
  // client-preference written by POST /api/channel/settings/companion).
  // Empty/missing = no binding → no companion greets this platform's channel.
  'assistant.telegram.companionId': string | undefined;
  'assistant.lark.agent':
    | { agent_type: string; backend?: string; id?: string; custom_agent_id?: string; name?: string }
    | undefined;
  'assistant.lark.companionId': string | undefined;
  'assistant.dingtalk.agent':
    | { agent_type: string; backend?: string; id?: string; custom_agent_id?: string; name?: string }
    | undefined;
  'assistant.dingtalk.companionId': string | undefined;
  'assistant.weixin.agent':
    | { agent_type: string; backend?: string; id?: string; custom_agent_id?: string; name?: string }
    | undefined;
  'assistant.weixin.companionId': string | undefined;
  'assistant.wecom.agent':
    | { agent_type: string; backend?: string; id?: string; custom_agent_id?: string; name?: string }
    | undefined;
  'assistant.wecom.companionId': string | undefined;
  'skillsMarket.enabled': boolean | undefined;
  // One-shot completion flags for legacy → backend migrations. Kept in the
  // local config file (not the backend client-preferences bag) so a downgrade
  // to a pre-flag build still re-reads the legacy data unchanged. See
  // `migrateProviders` / `migrateAssistantsToBackend` (ELECTRON-1KT).
  'migration.providersMigrated_v1': boolean | undefined;
  'migration.assistantsMigrated_v1': boolean | undefined;
};

export type ConfigKey = keyof ConfigKeyMap;
