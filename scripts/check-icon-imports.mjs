#!/usr/bin/env node
/**
 * icon-park 导入守卫 / @icon-park/react import guard
 *
 * 仓库规则:@icon-park/react 只能用「无别名的具名导入」。
 * 原因:构建期的图标包装插件会把 `import { X } from '@icon-park/react'`
 * 改写为 `X as _X` + HOC 包裹;源码里若已有别名(`Left as LeftArrow`),
 * 会被改写成 `Left as LeftArrow as _Left as LeftArrow` 这类非法语法,
 * 模块在 dev/build 阶段 500,而 tsc 完全无法发现(源码本身合法)。
 * 命名空间导入(`import * as Icons`)同理禁止。
 *
 * 扫描范围:ui/src 下全部 .ts/.tsx。
 * 用法 / Usage:
 *   bun scripts/check-icon-imports.mjs             # 校验,发现违规 exit 1
 *   bun scripts/check-icon-imports.mjs --self-test # 校验器自测
 */
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, dirname, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const SCAN_DIR = join(ROOT, 'ui', 'src');

// 具名导入块:import [type] { ... } from '@icon-park/react'(可跨行)
const NAMED_IMPORT_RE = /import\s+(?:type\s+)?\{([^}]*)\}\s*from\s*['"]@icon-park\/react['"]/g;
// 命名空间导入:import * as X from '@icon-park/react'
const NAMESPACE_IMPORT_RE = /import\s*\*\s*as\s+\w+\s*from\s*['"]@icon-park\/react['"]/g;
// 导入块内的别名:`Foo as Bar`(排除纯空白项)
const ALIAS_RE = /\b\w+\s+as\s+\w+/;

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    if (name === 'node_modules' || name === 'dist' || name.startsWith('.')) continue;
    const full = join(dir, name);
    const st = statSync(full);
    if (st.isDirectory()) yield* walk(full);
    else if (/\.tsx?$/.test(name)) yield full;
  }
}

function lineOf(source, index) {
  return source.slice(0, index).split('\n').length;
}

/** 返回违规清单 [{line, snippet, kind}] */
function scanSource(source) {
  const violations = [];
  for (const m of source.matchAll(NAMED_IMPORT_RE)) {
    const braces = m[1];
    if (ALIAS_RE.test(braces)) {
      violations.push({
        line: lineOf(source, m.index),
        snippet: m[0].replace(/\s+/g, ' ').slice(0, 120),
        kind: 'alias',
      });
    }
  }
  for (const m of source.matchAll(NAMESPACE_IMPORT_RE)) {
    violations.push({
      line: lineOf(source, m.index),
      snippet: m[0].replace(/\s+/g, ' ').slice(0, 120),
      kind: 'namespace',
    });
  }
  return violations;
}

function selfTest() {
  const cases = [
    { src: "import { Left } from '@icon-park/react';", bad: 0 },
    { src: "import { DeleteFour, Info, Left, PreviewOpen } from '@icon-park/react';", bad: 0 },
    { src: "import { Left as LeftArrow } from '@icon-park/react';", bad: 1 },
    { src: "import {\n  Cycle,\n  Play as Run,\n} from '@icon-park/react';", bad: 1 },
    { src: "import type { Icon as I } from '@icon-park/react';", bad: 1 },
    { src: "import * as Icons from '@icon-park/react';", bad: 1 },
    { src: "import { Left } from '@icon-park/svg';", bad: 0 }, // 别的包不管
    { src: "import { renderAs } from 'other'; // as 在别处", bad: 0 },
  ];
  let failed = 0;
  cases.forEach(({ src, bad }, i) => {
    const got = scanSource(src).length;
    if (got !== bad) {
      failed += 1;
      console.error(`self-test case ${i} failed: expected ${bad} violation(s), got ${got}\n  ${src}`);
    }
  });
  if (failed > 0) {
    console.error(`❌ check-icon-imports self-test: ${failed}/${cases.length} case(s) failed`);
    process.exit(1);
  }
  console.log(`✅ check-icon-imports self-test: ${cases.length}/${cases.length} cases pass`);
}

if (process.argv.includes('--self-test')) {
  selfTest();
  process.exit(0);
}

const problems = [];
let scanned = 0;
for (const file of walk(SCAN_DIR)) {
  const source = readFileSync(file, 'utf8');
  if (!source.includes('@icon-park/react')) continue;
  scanned += 1;
  for (const v of scanSource(source)) {
    problems.push({ file: relative(ROOT, file), ...v });
  }
}

if (problems.length > 0) {
  console.error('❌ @icon-park/react 导入违规(禁别名/禁命名空间导入,详见 scripts/check-icon-imports.mjs 头注):');
  for (const p of problems) {
    console.error(`  ${p.file}:${p.line} [${p.kind}] ${p.snippet}`);
  }
  process.exit(1);
}
console.log(`✅ icon-park imports clean (${scanned} file(s) using @icon-park/react)`);
