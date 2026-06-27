/**
 * @license
 * Copyright 2025-2026 NomiFun (nomifun.com)
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Button, Checkbox, Tabs } from '@arco-design/web-react';
import { ApiApp, Terminal, WebPage } from '@icon-park/react';
import CopyIconButton from '@/renderer/components/base/CopyIconButton';
import HubPageShell from '@/renderer/components/layout/HubPageShell';
import CompanionAccessTokenPanel from '@/renderer/components/layout/Sider/CompanionAccessTokenPanel';
import WebuiControlPanel from '@/renderer/components/layout/Sider/WebuiControlPanel';
import { WorkspaceFolderSelect } from '@/renderer/components/workspace';
import { useWebuiServer } from '@/renderer/hooks/context/WebuiServerContext';
import RegisterKnowledgeButton from '@/renderer/pages/terminal/RegisterKnowledgeButton';
import { WEBUI_DEFAULT_PORT } from '@/common/config/constants';

const formatJson = (value: unknown) => JSON.stringify(value, null, 2);

type OpenCapabilityTab = 'webui' | 'mcp';

type McpDomainOption = {
  id: string;
  titleKey: string;
  defaultTitle: string;
  descKey: string;
  defaultDesc: string;
};

const RECOMMENDED_MCP_DOMAINS = ['agent', 'conversation', 'browser', 'computer', 'knowledge', 'files', 'memory'];

const MCP_DOMAIN_OPTIONS: McpDomainOption[] = [
  {
    id: 'agent',
    titleKey: 'settings.openCapabilities.domainAgentTitle',
    defaultTitle: 'Agent 执行',
    descKey: 'settings.openCapabilities.domainAgentDesc',
    defaultDesc: '运行外部 Agent、检查 Agent 状态、读取可用模型和模式。',
  },
  {
    id: 'conversation',
    titleKey: 'settings.openCapabilities.domainConversationTitle',
    defaultTitle: '会话',
    descKey: 'settings.openCapabilities.domainConversationDesc',
    defaultDesc: '创建、读取、发送和管理 NomiFun 会话。',
  },
  {
    id: 'browser',
    titleKey: 'settings.openCapabilities.domainBrowserTitle',
    defaultTitle: '浏览器',
    descKey: 'settings.openCapabilities.domainBrowserDesc',
    defaultDesc: '让外部 Agent 观察网页、导航和执行浏览器动作。',
  },
  {
    id: 'computer',
    titleKey: 'settings.openCapabilities.domainComputerTitle',
    defaultTitle: '电脑控制',
    descKey: 'settings.openCapabilities.domainComputerDesc',
    defaultDesc: '让外部 Agent 使用桌面级电脑控制能力；未启用功能时不会暴露工具。',
  },
  {
    id: 'knowledge',
    titleKey: 'settings.openCapabilities.domainKnowledgeTitle',
    defaultTitle: '知识库',
    descKey: 'settings.openCapabilities.domainKnowledgeDesc',
    defaultDesc: '读取、写入、绑定和维护 NomiFun 知识库内容。',
  },
  {
    id: 'files',
    titleKey: 'settings.openCapabilities.domainFilesTitle',
    defaultTitle: '文件',
    descKey: 'settings.openCapabilities.domainFilesDesc',
    defaultDesc: '读写工作区文件、列目录和打开文件相关能力。',
  },
  {
    id: 'memory',
    titleKey: 'settings.openCapabilities.domainMemoryTitle',
    defaultTitle: '记忆',
    descKey: 'settings.openCapabilities.domainMemoryDesc',
    defaultDesc: '读取和维护伙伴记忆。',
  },
  {
    id: 'terminal',
    titleKey: 'settings.openCapabilities.domainTerminalTitle',
    defaultTitle: '终端',
    descKey: 'settings.openCapabilities.domainTerminalDesc',
    defaultDesc: '创建、查看和控制终端会话。',
  },
  {
    id: 'requirement',
    titleKey: 'settings.openCapabilities.domainRequirementTitle',
    defaultTitle: '需求',
    descKey: 'settings.openCapabilities.domainRequirementDesc',
    defaultDesc: '管理需求、任务工作区和需求相关自动化。',
  },
  {
    id: 'autowork',
    titleKey: 'settings.openCapabilities.domainAutoworkTitle',
    defaultTitle: 'AutoWork',
    descKey: 'settings.openCapabilities.domainAutoworkDesc',
    defaultDesc: '开启、关闭和查看需求 AutoWork 状态。',
  },
  {
    id: 'idmm',
    titleKey: 'settings.openCapabilities.domainIdmmTitle',
    defaultTitle: 'IDMM',
    descKey: 'settings.openCapabilities.domainIdmmDesc',
    defaultDesc: '模型调度、活动和智能分配相关能力。',
  },
  {
    id: 'cron',
    titleKey: 'settings.openCapabilities.domainCronTitle',
    defaultTitle: '计划任务',
    descKey: 'settings.openCapabilities.domainCronDesc',
    defaultDesc: '创建、读取和维护计划任务。',
  },
  {
    id: 'mcp',
    titleKey: 'settings.openCapabilities.domainMcpTitle',
    defaultTitle: 'MCP 服务',
    descKey: 'settings.openCapabilities.domainMcpDesc',
    defaultDesc: '读取和维护已接入的 MCP server 配置。',
  },
  {
    id: 'extension',
    titleKey: 'settings.openCapabilities.domainExtensionTitle',
    defaultTitle: '扩展',
    descKey: 'settings.openCapabilities.domainExtensionDesc',
    defaultDesc: '扩展包、扩展来源和扩展状态相关能力。',
  },
  {
    id: 'skill',
    titleKey: 'settings.openCapabilities.domainSkillTitle',
    defaultTitle: 'Skills',
    descKey: 'settings.openCapabilities.domainSkillDesc',
    defaultDesc: '读取、导入和管理技能包。',
  },
  {
    id: 'hub',
    titleKey: 'settings.openCapabilities.domainHubTitle',
    defaultTitle: 'Hub',
    descKey: 'settings.openCapabilities.domainHubDesc',
    defaultDesc: '浏览、安装和同步 Hub 能力。',
  },
  {
    id: 'system',
    titleKey: 'settings.openCapabilities.domainSystemTitle',
    defaultTitle: '系统',
    descKey: 'settings.openCapabilities.domainSystemDesc',
    defaultDesc: '系统设置、主题、模型提供商和客户端偏好。',
  },
  {
    id: 'provider',
    titleKey: 'settings.openCapabilities.domainProviderTitle',
    defaultTitle: '模型提供商',
    descKey: 'settings.openCapabilities.domainProviderDesc',
    defaultDesc: '读取模型提供商和可用模型信息。',
  },
  {
    id: 'companion',
    titleKey: 'settings.openCapabilities.domainCompanionTitle',
    defaultTitle: '伙伴',
    descKey: 'settings.openCapabilities.domainCompanionDesc',
    defaultDesc: '管理伙伴、绑定、远程访问令牌和伙伴资料。',
  },
  {
    id: 'channel',
    titleKey: 'settings.openCapabilities.domainChannelTitle',
    defaultTitle: '频道',
    descKey: 'settings.openCapabilities.domainChannelDesc',
    defaultDesc: '管理 IM 频道、配对、授权用户和伙伴绑定。',
  },
  {
    id: 'confirmation',
    titleKey: 'settings.openCapabilities.domainConfirmationTitle',
    defaultTitle: '确认队列',
    descKey: 'settings.openCapabilities.domainConfirmationDesc',
    defaultDesc: '读取和处理等待用户确认的动作。',
  },
];

const normalizeMcpDomains = (domains: string[]): string[] =>
  MCP_DOMAIN_OPTIONS.map((option) => option.id).filter((id) => domains.includes(id));

const OpenCapabilitiesPage: React.FC = () => {
  const { t } = useTranslation();
  const { accessUrls, lifecycleSupported, status } = useWebuiServer();
  const [cwd, setCwd] = useState('');
  const [activeOpenCapabilityTab, setActiveOpenCapabilityTab] = useState<OpenCapabilityTab>('webui');
  const [selectedMcpDomains, setSelectedMcpDomains] = useState<string[]>([...RECOMMENDED_MCP_DOMAINS]);

  const baseUrl = useMemo(() => {
    const preferred = accessUrls[0];
    if (preferred) return preferred.replace(/\/$/, '');
    const port = status?.port ?? WEBUI_DEFAULT_PORT;
    return `http://127.0.0.1:${port}`;
  }, [accessUrls, status?.port]);

  const mcpUrl = `${baseUrl}/mcp`;
  const restUrl = `${baseUrl}/v1`;
  const domainsQuery = useMemo(() => {
    const normalized = normalizeMcpDomains(selectedMcpDomains);
    return normalized.length === MCP_DOMAIN_OPTIONS.length ? '' : normalized.join(',');
  }, [selectedMcpDomains]);
  const querySuffix = domainsQuery ? `?domains=${domainsQuery}` : '';
  const selectedMcpUrl = `${mcpUrl}${querySuffix}`;
  const restToolsUrl = `${restUrl}/tools${querySuffix}`;
  const openapiUrl = `${restUrl}/openapi.json${querySuffix}`;
  const mcpClientJson = formatJson({
    mcpServers: {
      nomifun: {
        type: 'streamable-http',
        url: selectedMcpUrl,
        headers: {
          Authorization: 'Bearer <companion-access-token>',
        },
      },
    },
  });
  const restCurl = [
    `curl ${restToolsUrl}`,
    '  -H "Authorization: Bearer <companion-access-token>"',
  ].join(' \\\n');

  const selectedDomainCount = selectedMcpDomains.length;
  const toggleMcpDomain = (domain: string, checked: boolean) => {
    setSelectedMcpDomains((current) => {
      const next = checked ? [...current, domain] : current.filter((item) => item !== domain);
      const normalized = normalizeMcpDomains(Array.from(new Set(next)));
      return normalized.length > 0 ? normalized : current;
    });
  };

  return (
    <HubPageShell
      title={t('settings.openCapabilities.title', { defaultValue: '远程&开放能力' })}
      subtitle={t('settings.openCapabilities.subtitle', {
        defaultValue: '分开管理 WebUI 访问入口，以及 NomiFun Remote MCP / REST 对外开放能力。',
      })}
      maxWidthClass='md:max-w-1180px'
    >
      <Tabs
        activeTab={activeOpenCapabilityTab}
        onChange={(key) => setActiveOpenCapabilityTab(key as OpenCapabilityTab)}
        type='line'
        className='[&>.arco-tabs-content]:pt-16px'
      >
        <Tabs.TabPane key='webui' title={t('settings.openCapabilities.webuiTab', { defaultValue: 'WebUI 访问' })}>
          <section className='grid grid-cols-1 gap-16px lg:grid-cols-[minmax(0,1fr)_360px]'>
            <div className='rd-12px border border-border-2 bg-fill-0 p-16px'>
              <SectionHeader
                icon={<WebPage theme='outline' size='18' fill='currentColor' />}
                title={t('settings.openCapabilities.webuiTitle', { defaultValue: 'WebUI 远程访问' })}
                description={t('settings.openCapabilities.webuiDesc', {
                  defaultValue: '启用后，手机、平板或远程浏览器可以打开 NomiFun。二维码登录和账号密码都在这里处理。',
                })}
              />
              <div className='mt-14px'>
                <WebuiControlPanel mode='page' />
              </div>
            </div>

            <div className='flex flex-col gap-12px'>
              <InfoPanel
                icon={<ApiApp theme='outline' size='17' fill='currentColor' />}
                title={t('settings.openCapabilities.addressStrategyTitle', { defaultValue: '访问地址策略' })}
                body={t('settings.openCapabilities.addressStrategyDesc', {
                  defaultValue:
                    'NomiFun 只展示更可能被手机和局域网设备访问的地址；回环、链路本地、基准测试网段等地址不会进入二维码候选。',
                })}
              />
              <EndpointBlock
                label={t('settings.openCapabilities.currentAccessBase', { defaultValue: '当前访问基址' })}
                value={baseUrl}
              />
              <CapabilityNote
                title={t('settings.openCapabilities.ipFilterTitle', { defaultValue: '会隐藏哪些 IP' })}
                body={t('settings.openCapabilities.ipFilterDesc', {
                  defaultValue:
                    '隐藏 127.0.0.1、0.0.0.0、169.254.*、198.18.* / 198.19.*、组播和文档保留地址；保留 192.168.*、10.*、172.16-31.* 等可用 LAN/VPN 候选。',
                })}
              />
            </div>
          </section>
        </Tabs.TabPane>

        <Tabs.TabPane key='mcp' title={t('settings.openCapabilities.mcpTab', { defaultValue: 'MCP 能力' })}>
          <div className='flex flex-col gap-16px'>
            <section className='grid grid-cols-1 gap-16px lg:grid-cols-[minmax(0,1fr)_360px]'>
              <div className='rd-12px border border-border-2 bg-fill-0 p-16px'>
                <SectionHeader
                  icon={<ApiApp theme='outline' size='18' fill='currentColor' />}
                  title={t('settings.openCapabilities.remoteTitle', { defaultValue: 'NomiFun Remote MCP 能力范围' })}
                  description={t('settings.openCapabilities.remoteDesc', {
                    defaultValue:
                      '勾选外部 Agent 通过 NomiFun Remote MCP / REST 能看到和调用的平台能力。配置会体现在生成的接入 URL 中。',
                  })}
                />

                <div className='mt-14px flex flex-col gap-10px md:flex-row md:items-center md:justify-between'>
                  <div className='text-12px leading-18px text-t-secondary'>
                    {t('settings.openCapabilities.selectedDomainCount', {
                      count: selectedDomainCount,
                      total: MCP_DOMAIN_OPTIONS.length,
                      defaultValue: '已选择 {{count}} / {{total}} 个能力域',
                    })}
                  </div>
                  <div className='flex flex-wrap gap-8px'>
                    <Button size='mini' onClick={() => setSelectedMcpDomains([...RECOMMENDED_MCP_DOMAINS])}>
                      {t('settings.openCapabilities.selectRecommendedDomains', { defaultValue: '推荐范围' })}
                    </Button>
                    <Button size='mini' onClick={() => setSelectedMcpDomains(MCP_DOMAIN_OPTIONS.map((option) => option.id))}>
                      {t('settings.openCapabilities.selectAllDomains', { defaultValue: '全部能力' })}
                    </Button>
                  </div>
                </div>

                <div className='mt-12px grid grid-cols-1 gap-10px md:grid-cols-2 xl:grid-cols-3'>
                  {MCP_DOMAIN_OPTIONS.map((option) => {
                    const checked = selectedMcpDomains.includes(option.id);
                    return (
                      <DomainOptionCard
                        key={option.id}
                        id={option.id}
                        title={t(option.titleKey, { defaultValue: option.defaultTitle })}
                        description={t(option.descKey, { defaultValue: option.defaultDesc })}
                        checked={checked}
                        onChange={(nextChecked) => toggleMcpDomain(option.id, nextChecked)}
                      />
                    );
                  })}
                </div>
              </div>

              <div className='flex flex-col gap-12px'>
                <InfoPanel
                  icon={<Terminal theme='outline' size='17' fill='currentColor' />}
                  title={t('settings.openCapabilities.generatedConfigTitle', { defaultValue: '当前生成的接入配置' })}
                  body={t('settings.openCapabilities.generatedConfigDesc', {
                    defaultValue:
                      '把下面的 MCP 地址写入外部 Agent。URL 中的 domains 参数就是当前勾选的 NomiFun 平台能力范围。',
                  })}
                />
                <EndpointBlock
                  label={t('settings.openCapabilities.mcpEndpoint', { defaultValue: 'MCP 地址' })}
                  value={selectedMcpUrl}
                />
                <EndpointBlock
                  label={t('settings.openCapabilities.restToolsEndpoint', { defaultValue: 'REST 工具列表' })}
                  value={restToolsUrl}
                />
                <EndpointBlock
                  label={t('settings.openCapabilities.openapiEndpoint', { defaultValue: 'OpenAPI' })}
                  value={openapiUrl}
                />
                {lifecycleSupported && (
                  <div className='rd-12px border border-border-2 bg-fill-0 p-14px'>
                    <CompanionAccessTokenPanel />
                  </div>
                )}
              </div>
            </section>

            <section className='grid grid-cols-1 gap-16px lg:grid-cols-2'>
              <SnippetBlock
                label={t('settings.openCapabilities.mcpClientConfig', { defaultValue: 'MCP 客户端配置示例' })}
                code={mcpClientJson}
              />
              <SnippetBlock
                label={t('settings.openCapabilities.restExample', { defaultValue: 'REST 调用示例' })}
                code={restCurl}
              />
            </section>

            <section className='rd-12px border border-border-2 bg-fill-0 p-16px'>
              <SectionHeader
                icon={<Terminal theme='outline' size='18' fill='currentColor' />}
                title={t('settings.openCapabilities.projectRegisterTitle', { defaultValue: '项目级 MCP 注册' })}
                description={t('settings.openCapabilities.projectRegisterDesc', {
                  defaultValue:
                    '当前可一键写入的是平台知识库 MCP：Claude / Gemini 写入指定项目配置，Codex 由于没有项目级配置，只能写入全局配置。',
                })}
              />
              <div className='mt-14px grid grid-cols-1 gap-12px lg:grid-cols-[minmax(0,1fr)_auto] lg:items-end'>
                <div>
                  <div className='mb-6px text-13px font-500 text-t-primary'>
                    {t('settings.openCapabilities.projectPath', { defaultValue: '目标项目' })}
                  </div>
                  <WorkspaceFolderSelect
                    value={cwd}
                    onChange={setCwd}
                    onClear={() => setCwd('')}
                    placeholder={t('terminal.create.workspacePlaceholder')}
                    recentLabel={t('terminal.create.recent')}
                    chooseDifferentLabel={t('terminal.create.chooseFolder')}
                  />
                </div>
                <div className='lg:pb-1px'>
                  <RegisterKnowledgeButton cwd={cwd} command='claude' />
                </div>
              </div>
              <div className='mt-12px grid grid-cols-1 gap-8px md:grid-cols-3'>
                <CapabilityNote
                  title='Claude'
                  body={t('settings.openCapabilities.projectRegisterClaude', {
                    defaultValue: '写入项目 .mcp.json，适合随项目启动 Claude Code。',
                  })}
                />
                <CapabilityNote
                  title='Gemini'
                  body={t('settings.openCapabilities.projectRegisterGemini', {
                    defaultValue: '写入项目 .gemini/settings.json，保留已有 mcpServers。',
                  })}
                />
                <CapabilityNote
                  title='Codex'
                  body={t('settings.openCapabilities.projectRegisterCodex', {
                    defaultValue: '当前只能调用 codex mcp add 写入全局 ~/.codex/config.toml。',
                  })}
                />
              </div>
              <div className='mt-12px rd-10px border border-[rgba(var(--primary-6),0.22)] bg-[rgba(var(--primary-6),0.06)] px-12px py-10px text-12px leading-18px text-t-secondary'>
                {t('settings.openCapabilities.oneClickPlan', {
                  defaultValue:
                    '完整 Remote MCP 的一键项目注册可以沿用这个模式：选择项目与客户端，NomiFun 生成或复用伙伴令牌，再写入项目级 MCP 配置。由于 Remote MCP 需要 Bearer token，落地前应让用户明确确认是否把令牌写进项目文件，或改为写入环境变量引用。',
                })}
              </div>
            </section>
          </div>
        </Tabs.TabPane>
      </Tabs>
    </HubPageShell>
  );
};

const SectionHeader: React.FC<{ icon: React.ReactElement; title: string; description: string }> = ({
  icon,
  title,
  description,
}) => (
  <div className='flex items-start gap-10px'>
    <span className='mt-1px flex size-30px shrink-0 items-center justify-center rd-8px bg-primary-1 text-primary-6'>
      {icon}
    </span>
    <div className='min-w-0'>
      <div className='text-15px font-600 leading-22px text-t-primary'>{title}</div>
      <div className='mt-3px text-12px leading-18px text-t-tertiary'>{description}</div>
    </div>
  </div>
);

const DomainOptionCard: React.FC<{
  id: string;
  title: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}> = ({ id, title, description, checked, onChange }) => (
  <label
    className={`block cursor-pointer rd-10px border px-11px py-10px transition-colors ${
      checked
        ? 'border-[rgba(var(--primary-6),0.32)] bg-[rgba(var(--primary-6),0.06)]'
        : 'border-border-2 bg-fill-1 hover:border-border-3 hover:bg-fill-2'
    }`}
  >
    <div className='flex items-start gap-9px'>
      <Checkbox checked={checked} onChange={onChange} className='mt-2px shrink-0' />
      <div className='min-w-0'>
        <div className='flex items-center gap-6px'>
          <span className='text-13px font-600 leading-19px text-t-primary'>{title}</span>
          <code className='truncate rd-6px bg-fill-0 px-5px py-1px font-mono text-10px text-t-tertiary'>{id}</code>
        </div>
        <div className='mt-4px text-12px leading-17px text-t-tertiary'>{description}</div>
      </div>
    </div>
  </label>
);

const InfoPanel: React.FC<{ icon: React.ReactElement; title: string; body: string }> = ({ icon, title, body }) => (
  <div className='rd-12px border border-border-2 bg-fill-0 p-14px'>
    <div className='flex items-center gap-8px text-14px font-600 text-t-primary'>
      <span className='text-primary-6'>{icon}</span>
      {title}
    </div>
    <div className='mt-6px text-12px leading-18px text-t-tertiary'>{body}</div>
  </div>
);

const EndpointBlock: React.FC<{ label: string; value: string }> = ({ label, value }) => (
  <div className='rd-10px border border-border-2 bg-fill-0 px-12px py-10px'>
    <div className='mb-5px text-12px font-500 text-t-tertiary'>{label}</div>
    <div className='flex items-center gap-8px'>
      <code className='min-w-0 flex-1 truncate font-mono text-12px text-t-primary'>{value}</code>
      <CopyIconButton text={value} size={14} className='size-22px shrink-0' />
    </div>
  </div>
);

const SnippetBlock: React.FC<{ label: string; code: string }> = ({ label, code }) => (
  <div className='rd-10px border border-border-2 bg-fill-0 px-12px py-10px'>
    <div className='mb-6px flex items-center justify-between gap-8px'>
      <span className='text-12px font-500 text-t-tertiary'>{label}</span>
      <CopyIconButton text={code} size={14} className='size-22px shrink-0' />
    </div>
    <pre className='m-0 max-h-180px overflow-auto whitespace-pre-wrap break-all rd-8px bg-fill-2 px-10px py-8px font-mono text-11px leading-16px text-t-primary'>
      {code}
    </pre>
  </div>
);

const CapabilityNote: React.FC<{ title: string; body: string }> = ({ title, body }) => (
  <div className='rd-10px border border-border-2 bg-fill-1 px-12px py-10px'>
    <div className='text-13px font-600 text-t-primary'>{title}</div>
    <div className='mt-4px text-12px leading-17px text-t-tertiary'>{body}</div>
  </div>
);

export default OpenCapabilitiesPage;
