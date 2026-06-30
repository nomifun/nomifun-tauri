#!/usr/bin/env bun
/**
 * bump-version — 一条命令统一改版本号。
 *
 *   bun run bump 1.2.3            # 写入版本号 + 同步 Cargo.lock
 *   bun run bump 1.2.3 --tag      # 额外: git commit + git tag v1.2.3（需干净工作树）
 *
 * 单一真源 = 根 Cargo.toml 的 [workspace.package].version：后端的
 * CARGO_PKG_VERSION / app_version 跟随它，apps/desktop/tauri.conf.json 通过
 * 省略自己的 version 字段来继承它（安装包文件名 / 更新器版本随之）。本脚本把这条
 * 真源写好，并把装饰性的 package.json / ui/package.json 拉齐、刷新 Cargo.lock；
 * 若 tauri.conf.json 哪天又写回显式 version，也会一并更新。
 *
 * 纯 node:fs/child_process，无第三方依赖；改 Cargo.toml 用行级替换以保留注释。
 */
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { execSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

const argv = process.argv.slice(2);
const wantTag = argv.includes('--tag');
const version = argv.find((a) => !a.startsWith('-'));

// SemVer（含可选 prerelease / build 元数据）。
const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
if (!version || !SEMVER.test(version)) {
  console.error('用法: bun run bump <x.y.z> [--tag]');
  console.error(version ? `  无效版本号: ${version}` : '  缺少版本号参数。');
  process.exit(1);
}

// --tag 需要干净工作树（提交里只应有本次 bump 的改动）。
if (wantTag) {
  const dirty = execSync('git status --porcelain', { cwd: ROOT, encoding: 'utf8' }).trim();
  if (dirty) {
    console.error('✗ --tag 需要干净的工作树（先提交或暂存现有改动）。当前未提交改动:');
    console.error(dirty);
    process.exit(1);
  }
}

let changed = 0;
const rel = (p) => p.slice(ROOT.length + 1);
function report(p, oldVal, noop = false) {
  if (noop) console.log(`  = ${rel(p)} 已是 ${version}`);
  else {
    console.log(`  ✓ ${rel(p)}: ${oldVal} → ${version}`);
    changed++;
  }
}

// 1) 单一真源：根 Cargo.toml 的 [workspace.package].version（行级替换，保留注释/格式）。
function bumpCargoToml() {
  const p = join(ROOT, 'Cargo.toml');
  const lines = readFileSync(p, 'utf8').split('\n');
  let inSection = false;
  for (let i = 0; i < lines.length; i++) {
    const t = lines[i].trim();
    if (t.startsWith('[')) {
      inSection = t === '[workspace.package]';
      continue;
    }
    if (inSection) {
      const m = lines[i].match(/^(\s*version\s*=\s*")([^"]*)(".*)$/);
      if (m) {
        if (m[2] === version) report(p, m[2], true);
        else {
          lines[i] = `${m[1]}${version}${m[3]}`;
          report(p, m[2]);
          writeFileSync(p, lines.join('\n'));
        }
        return;
      }
    }
  }
  console.error(`✗ 未在 ${rel(p)} 的 [workspace.package] 段找到 version 行`);
  process.exit(1);
}

// 2) JSON 清单：替换首个顶层 "version": "..."（行级，最小 diff）。required=false 时缺字段就跳过。
function bumpJson(relPath, required) {
  const p = join(ROOT, relPath);
  if (!existsSync(p)) {
    if (required) {
      console.error(`✗ 缺少文件 ${relPath}`);
      process.exit(1);
    }
    return;
  }
  const src = readFileSync(p, 'utf8');
  const m = src.match(/("version"\s*:\s*")([^"]*)(")/);
  if (!m) {
    if (required) {
      console.error(`✗ ${relPath} 无 version 字段`);
      process.exit(1);
    }
    return;
  }
  if (m[2] === version) {
    report(p, m[2], true);
    return;
  }
  writeFileSync(p, src.replace(m[0], `${m[1]}${version}${m[3]}`));
  report(p, m[2]);
}

console.log(`\n▶ 统一版本号 → ${version}\n`);
bumpCargoToml();
bumpJson('package.json', true);
bumpJson('ui/package.json', true);
// tauri.conf.json 默认靠继承（无 version 字段）；若存在显式 version 才更新，否则跳过。
bumpJson('apps/desktop/tauri.conf.json', false);

// 3) 同步 Cargo.lock 里 workspace 成员的版本（否则 --locked 构建会抱怨）。
try {
  console.log('\n▶ 同步 Cargo.lock（cargo update --workspace）...');
  execSync('cargo update --workspace', { cwd: ROOT, stdio: 'inherit' });
} catch (e) {
  console.warn('⚠️  cargo update --workspace 失败（不阻断）；下次 cargo build 会自动同步 Cargo.lock。');
}

// 4) 可选 git tag。
if (wantTag) {
  console.log(`\n▶ 提交并打 tag v${version} ...`);
  execSync('git add -A', { cwd: ROOT, stdio: 'inherit' });
  execSync(`git commit -m "chore(release): v${version}"`, { cwd: ROOT, stdio: 'inherit' });
  execSync(`git tag v${version}`, { cwd: ROOT, stdio: 'inherit' });
  console.log(`✓ 已提交并打 tag v${version}（记得 git push origin main --tags 推送）。`);
}

console.log(
  `\n✅ 完成：${changed > 0 ? `${changed} 处更新到 ${version}` : `已全部是 ${version}（无改动）`}。` +
    `\n   提示：tauri.conf.json 通过省略 version 继承 Cargo workspace 版本，安装包/更新器版本随之统一。\n`
);
