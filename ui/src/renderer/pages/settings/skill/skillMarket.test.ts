import { describe, expect, test } from 'bun:test';
import {
  buildSkillMarketInstallPrompt,
  filterSkillMarketItems,
  normalizeSkillMarketItem,
  normalizeSkillMarketItems,
  translateMarketDescription,
} from './skillMarket';

const item = {
  id: 'clawhub:owner/demo',
  source: 'clawhub' as const,
  rank: 1,
  name: 'demo skill',
  description: 'GitHub coding helper',
  url: 'https://clawhub.ai/owner/skills/demo',
  install_command: 'openclaw skills install @owner/demo',
  tags: ['developer', 'coding'],
  audience_tags: ['developer'],
  scenario_tags: ['coding'],
};

describe('skill market helpers', () => {
  test('filters by source, search, and shared tags', () => {
    const result = filterSkillMarketItems([item], 'clawhub', 'github', {
      audience: ['developer'],
      scenario: ['coding'],
    });

    expect(result).toEqual([item]);
    expect(filterSkillMarketItems([item], 'shillhub', '', { audience: [], scenario: [] })).toHaveLength(0);
    expect(filterSkillMarketItems([item], 'clawhub', 'missing', { audience: [], scenario: [] })).toHaveLength(0);
    expect(filterSkillMarketItems([item], 'clawhub', '开发', { audience: [], scenario: [] })).toEqual([item]);
  });

  test('rejects unsafe cached commands and URLs', () => {
    expect(
      normalizeSkillMarketItem({
        ...item,
        install_command: 'openclaw skills install @owner/demo; rm -rf ~',
      })
    ).toBeNull();
    expect(normalizeSkillMarketItem({ ...item, url: 'https://example.com/owner/demo' })).toBeNull();
    expect(normalizeSkillMarketItems([item, { bad: true }])).toHaveLength(1);
  });

  test('builds a draft prompt containing the install command', () => {
    const prompt = buildSkillMarketInstallPrompt(item);
    expect(prompt.includes('请帮我安装这个技能')).toBe(true);
    expect(prompt.includes('openclaw skills install @owner/demo')).toBe(true);
    expect(prompt.includes('https://clawhub.ai/owner/skills/demo')).toBe(true);
  });

  test('translates common market descriptions for zh display', () => {
    expect(translateMarketDescription('Ranked Skills.sh skill from vercel-labs/skills.', item)).toBe(
      '来自 vercel-labs/skills 的 Skills.sh 榜单技能。'
    );
    expect(translateMarketDescription('GitHub coding helper', item).includes('开发')).toBe(true);
  });
});
