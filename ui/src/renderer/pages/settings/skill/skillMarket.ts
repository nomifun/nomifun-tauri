import type { ISkillMarketItem, SkillMarketSource } from '@/common/adapter/ipcBridge';
import type { SkillTagFilterState } from './skillFilter';

export const SKILL_MARKET_SOURCES: SkillMarketSource[] = ['clawhub', 'shillhub'];

const MAX_NAME_LENGTH = 96;
const MAX_DESCRIPTION_LENGTH = 220;
const MAX_COMMAND_LENGTH = 320;

export const isSkillMarketSource = (value: unknown): value is SkillMarketSource =>
  value === 'clawhub' || value === 'shillhub';

export const cleanMarketText = (value: unknown, maxLength = MAX_DESCRIPTION_LENGTH): string => {
  if (typeof value !== 'string') return '';
  return value
    .replace(/[\u0000-\u001f\u007f]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, maxLength);
};

const isSafeMarketUrl = (source: SkillMarketSource, url: string): boolean => {
  if (source === 'clawhub') return url.startsWith('https://clawhub.ai/');
  return url.startsWith('https://www.skills.sh/') || url.startsWith('https://skills.sh/');
};

const isSafeInstallCommand = (value: string): boolean => {
  if (!value || value.length > MAX_COMMAND_LENGTH) return false;
  if (/[\r\n;&|<>`$]/.test(value)) return false;
  return value.startsWith('openclaw skills install @') || value.startsWith('npx skills add ');
};

const cleanTagList = (value: unknown): string[] => {
  if (!Array.isArray(value)) return [];
  const seen = new Set<string>();
  return value
    .map((item) => cleanMarketText(item, 40).toLowerCase())
    .filter((item) => /^[a-z0-9_-]+$/.test(item))
    .filter((item) => {
      if (seen.has(item)) return false;
      seen.add(item);
      return true;
    })
    .slice(0, 12);
};

export const normalizeSkillMarketItem = (raw: unknown): ISkillMarketItem | null => {
  if (!raw || typeof raw !== 'object') return null;
  const data = raw as Partial<ISkillMarketItem>;
  if (!isSkillMarketSource(data.source)) return null;

  const url = cleanMarketText(data.url, 260);
  const install_command = cleanMarketText(data.install_command, MAX_COMMAND_LENGTH);
  if (!isSafeMarketUrl(data.source, url) || !isSafeInstallCommand(install_command)) return null;

  const name = cleanMarketText(data.name, MAX_NAME_LENGTH);
  if (!name) return null;

  return {
    id: cleanMarketText(data.id, 160) || `${data.source}:${name}`,
    source: data.source,
    rank: Number.isFinite(data.rank) ? Number(data.rank) : 0,
    name,
    description: cleanMarketText(data.description, MAX_DESCRIPTION_LENGTH),
    url,
    install_command,
    tags: cleanTagList(data.tags),
    audience_tags: cleanTagList(data.audience_tags),
    scenario_tags: cleanTagList(data.scenario_tags),
    stats: cleanMarketText(data.stats, 60) || undefined,
  };
};

export const normalizeSkillMarketItems = (raw: unknown): ISkillMarketItem[] => {
  if (!Array.isArray(raw)) return [];
  return raw.map(normalizeSkillMarketItem).filter((item): item is ISkillMarketItem => Boolean(item));
};

export const translateMarketDescription = (
  description: string,
  item?: Pick<ISkillMarketItem, 'name' | 'source' | 'audience_tags' | 'scenario_tags'>,
  localeKey = 'zh-CN'
): string => {
  const text = cleanMarketText(description, MAX_DESCRIPTION_LENGTH);
  if (!localeKey.toLowerCase().startsWith('zh')) return text;
  if (!text || /[\u4e00-\u9fff]/.test(text)) return text;

  const skillsSh = text.match(/^Ranked Skills\.sh skill from ([\w.-]+)\/skills\.$/i);
  if (skillsSh) return `来自 ${skillsSh[1]}/skills 的 Skills.sh 榜单技能。`;

  const lower = text.toLowerCase();
  if (lower.includes('security') && lower.includes('skill')) {
    return '用于安装前审查技能安全性，帮助识别风险和不可信内容。';
  }
  if (lower.includes('github')) return '用于 GitHub、代码仓库和开发协作流程的技能。';
  if (lower.includes('pdf')) return '用于 PDF 文档读取、分析和处理的技能。';
  if (lower.includes('weather')) return '用于查询天气、预报和相关环境信息的技能。';
  if (lower.includes('search')) return '用于联网搜索、资料检索和信息整理的技能。';
  if (lower.includes('self') && lower.includes('improv')) return '用于记录经验、错误和修正，帮助 Agent 持续改进。';

  const tags = new Set([...(item?.audience_tags ?? []), ...(item?.scenario_tags ?? [])]);
  const name = cleanMarketText(item?.name, 40) || '该技能';
  if (tags.has('coding')) return `${name} 用于代码、CLI 或开发自动化工作流。`;
  if (tags.has('document')) return `${name} 用于文档、写作或办公文件处理。`;
  if (tags.has('spreadsheet')) return `${name} 用于表格、数据整理或办公分析。`;
  if (tags.has('presentation')) return `${name} 用于演示文稿制作或幻灯片处理。`;
  if (tags.has('design')) return `${name} 用于设计、图片或创意生产工作流。`;
  if (tags.has('research')) return `${name} 用于学术研究、资料检索和内容归纳。`;
  if (tags.has('planning')) return `${name} 用于任务规划、项目推进和流程管理。`;
  if (tags.has('social')) return `${name} 用于社交媒体、内容发布或营销工作流。`;
  if (tags.has('setup')) return `${name} 用于工具配置、安装或初始化流程。`;

  return `${name} 的市场榜单技能，可扩展 Nomi 的自动化能力。`;
};

export const filterSkillMarketItems = (
  items: ISkillMarketItem[],
  source: SkillMarketSource,
  query: string,
  tagFilter: SkillTagFilterState
): ISkillMarketItem[] => {
  const q = query.trim().toLowerCase();
  return items.filter((item) => {
    if (item.source !== source) return false;
    if (q) {
      const haystack = [
        item.name,
        item.description,
        translateMarketDescription(item.description, item, 'zh-CN'),
        item.tags?.join(' '),
        item.stats,
      ]
        .join(' ')
        .toLowerCase();
      if (!haystack.includes(q)) return false;
    }
    if (tagFilter.audience.length > 0) {
      const itemTags = new Set(item.audience_tags ?? []);
      if (!tagFilter.audience.some((tag) => itemTags.has(tag))) return false;
    }
    if (tagFilter.scenario.length > 0) {
      const itemTags = new Set(item.scenario_tags ?? []);
      if (!tagFilter.scenario.some((tag) => itemTags.has(tag))) return false;
    }
    return true;
  });
};

export const buildSkillMarketConversationName = (item: ISkillMarketItem): string =>
  `安装 ${cleanMarketText(item.name, 48)}`;

export const buildSkillMarketInstallPrompt = (item: ISkillMarketItem): string => {
  const name = cleanMarketText(item.name, MAX_NAME_LENGTH);
  const source = item.source === 'clawhub' ? 'ClawHub' : 'Skills.sh';
  const description = translateMarketDescription(item.description, item, 'zh-CN');
  return [
    '请帮我安装这个技能。先检查来源页面和安装命令是否可信，执行前向我确认。',
    '',
    `来源：${source}`,
    `技能：${name}`,
    description ? `说明：${description}` : null,
    `页面：${item.url}`,
    '',
    '安装命令：',
    '```bash',
    item.install_command,
    '```',
  ]
    .filter(Boolean)
    .join('\n');
};
