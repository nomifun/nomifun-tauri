#!/usr/bin/env bun
/**
 * make-latest-json — 生成 / 合并 Tauri 自动更新清单 latest.json。
 *
 *   bun run make:latest                        # 扫本机 target/ 里的更新产物，合并进清单
 *   bun run make:latest --version 0.1.11        # 显式指定版本（默认读单一真源）
 *   bun run make:latest --notes "修复若干问题"    # 指定发布说明（默认读 CHANGELOG / 兜底）
 *   bun run make:latest --repo owner/name       # 指定 GitHub 仓库（默认 nomifun/nomifun-tauri）
 *   bun run make:latest --collect               # 额外把产物 + .sig 拷到 dist/desktop/ 便于上传
 *
 * 背景：Tauri 自动更新靠一个 latest.json 清单，按 `<系统>-<芯片>` 列出每个平台的下载
 * 地址(url) + minisign 签名(signature)。各平台**不能交叉编译**，所以本脚本设计成「在
 * 哪台机器跑就补哪个平台的条目」，**合并**进同一个 latest.json：Mac 上补 darwin-*，
 * Windows 上补 windows-*，最终汇总成完整清单（见 apps/desktop/updater/README.md）。
 *
 * 流程：
 *   1) 扫 target/**\/release/bundle/ 下的更新产物（每个产物旁有一个 .sig）。
 *   2) 由所在 target triple（或默认 host 构建）推断平台键，读 .sig 内容。
 *   3) url 指向 GitHub Releases 的版本化资产地址。
 *   4) 读入既有 latest.json（保留其它平台条目），更新 version/notes/pub_date + 本机条目，写回。
 *
 * 单一真源 = 根 Cargo.toml [workspace.package].version。纯 node:fs，无第三方依赖。
 */
import { readFileSync, writeFileSync, existsSync, readdirSync, statSync, mkdirSync, copyFileSync } from 'node:fs';
import { dirname, join, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const TARGET = join(ROOT, 'target');
const DEFAULT_OUT = join(ROOT, 'apps/desktop/updater/latest.json');
const DEFAULT_REPO = 'nomifun/nomifun-tauri';
const ALL_KEYS = ['windows-x86_64', 'windows-aarch64', 'darwin-x86_64', 'darwin-aarch64', 'linux-x86_64', 'linux-aarch64'];

const rel = (p) => (p.startsWith(ROOT) ? p.slice(ROOT.length + 1) : p);

// ── 参数解析（--flag value / 无值开关返回 true） ──────────────────────────────
const argv = process.argv.slice(2);
function flag(name, fallback = undefined) {
  const i = argv.indexOf(`--${name}`);
  if (i === -1) return fallback;
  const next = argv[i + 1];
  return next && !next.startsWith('--') ? next : true;
}

const repo = flag('repo', DEFAULT_REPO);
const out = flag('out', DEFAULT_OUT);
const collect = flag('collect', false) === true;
const version = flag('version') || readWorkspaceVersion();
const notesFile = flag('notes-file');
const notesFromFile = typeof notesFile === 'string' && existsSync(notesFile) ? readFileSync(notesFile, 'utf8').trim() : null;
const notes = flag('notes') || notesFromFile || readChangelogNotes(version) || `NomiFun v${version}`;
const distDir = join(ROOT, 'dist/desktop');

// 单一真源版本号：根 Cargo.toml 的 [workspace.package].version。
function readWorkspaceVersion() {
  const lines = readFileSync(join(ROOT, 'Cargo.toml'), 'utf8').split('\n');
  let inSection = false;
  for (const line of lines) {
    const t = line.trim();
    if (t.startsWith('[')) {
      inSection = t === '[workspace.package]';
      continue;
    }
    if (inSection) {
      const m = line.match(/^\s*version\s*=\s*"([^"]+)"/);
      if (m) return m[1];
    }
  }
  console.error('✗ 无法从根 Cargo.toml 读取 [workspace.package].version');
  process.exit(1);
}

// 发布说明：从 CHANGELOG.md 取当前版本小节的正文。优先匹配标题里含本次 version 的
// 小节（如 `## v0.1.13 - ...`）；匹配不到则退回第一个**非 Unreleased** 小节。跳过
// `## Unreleased`（占位内容如 "No unreleased changes yet."）——补发平台包时若误取它，
// 会把 latest.json 里已写好的发布说明覆盖成占位。读不到返回 null。
function readChangelogNotes(version) {
  const p = join(ROOT, 'CHANGELOG.md');
  if (!existsSync(p)) return null;
  const lines = readFileSync(p, 'utf8').split('\n');
  const heads = [];
  for (let i = 0; i < lines.length; i++) {
    if (/^##\s+/.test(lines[i])) heads.push(i);
  }
  if (heads.length === 0) return null;
  const isUnreleased = (i) => /^##\s+unreleased\b/i.test(lines[i]);
  const namesVersion = (i) => version && lines[i].includes(version);
  let start = heads.find(namesVersion);
  if (start === undefined) start = heads.find((i) => !isUnreleased(i));
  if (start === undefined) return null;
  const body = [];
  for (let i = start + 1; i < lines.length; i++) {
    if (/^##\s+/.test(lines[i])) break;
    body.push(lines[i]);
  }
  const text = body.join('\n').trim();
  return text || null;
}

// triple → 平台键。一个 universal mac 包同时服务两个 darwin 芯片。
function platformKeysFor(triple) {
  if (triple.includes('apple-darwin')) {
    if (triple.includes('universal')) return ['darwin-x86_64', 'darwin-aarch64'];
    return [triple.includes('aarch64') ? 'darwin-aarch64' : 'darwin-x86_64'];
  }
  if (triple.includes('windows')) return [triple.includes('aarch64') ? 'windows-aarch64' : 'windows-x86_64'];
  if (triple.includes('linux')) return [triple.includes('aarch64') ? 'linux-aarch64' : 'linux-x86_64'];
  return [];
}

// 默认（无 --target）构建落在 target/release/bundle，其 triple = 本机 host triple。
function hostTriple() {
  const arch = process.arch === 'arm64' ? 'aarch64' : process.arch === 'x64' ? 'x86_64' : process.arch;
  if (process.platform === 'darwin') return `${arch}-apple-darwin`;
  if (process.platform === 'win32') return `${arch}-pc-windows-msvc`;
  return `${arch}-unknown-linux-gnu`;
}

function listDirs(p) {
  if (!existsSync(p)) return [];
  return readdirSync(p).filter((e) => statSync(join(p, e)).isDirectory());
}

// 在一个 bundle 目录下递归找 *.sig，配对出更新产物（Tauri 只为更新产物写 .sig）。
function findSigs(bundleDir) {
  const found = [];
  const walk = (dir) => {
    for (const e of readdirSync(dir)) {
      const full = join(dir, e);
      if (statSync(full).isDirectory()) walk(full);
      else if (e.endsWith('.sig')) {
        const artifact = full.slice(0, -4);
        if (existsSync(artifact)) found.push({ artifact, sig: full });
      }
    }
  };
  walk(bundleDir);
  return found;
}

// ── 扫描 target/ ────────────────────────────────────────────────────────────
if (!existsSync(TARGET)) {
  console.error(`✗ 找不到 target/（${rel(TARGET)}）。先构建更新产物：bun run build:updater`);
  process.exit(1);
}

// 候选 bundle 目录：target/release/bundle（默认 host 构建）+ target/<triple>/release/bundle（指定 target）。
const bundleDirs = [];
const directDefault = join(TARGET, 'release', 'bundle');
if (existsSync(directDefault)) bundleDirs.push({ dir: directDefault, triple: hostTriple() });
for (const entry of listDirs(TARGET)) {
  if (entry === 'release' || entry === 'debug') continue;
  const nested = join(TARGET, entry, 'release', 'bundle');
  if (existsSync(nested)) bundleDirs.push({ dir: nested, triple: entry });
}

const collected = {}; // platformKey -> { url, signature, artifact, sig }
const uploads = new Set();
for (const { dir, triple } of bundleDirs) {
  const keys = platformKeysFor(triple);
  if (keys.length === 0) {
    console.warn(`  ! 跳过无法识别的 triple: ${triple}`);
    continue;
  }
  for (const { artifact, sig } of findSigs(dir)) {
    const name = basename(artifact);
    const signature = readFileSync(sig, 'utf8').trim();
    const url = `https://github.com/${repo}/releases/download/v${version}/${name}`;
    for (const key of keys) {
      if (collected[key]) {
        console.warn(`  ! ${key} 多个候选产物，后者覆盖：${basename(collected[key].artifact)} → ${name}`);
      }
      collected[key] = { url, signature, artifact, sig };
    }
    uploads.add(artifact);
    uploads.add(sig);
  }
}

const foundKeys = Object.keys(collected);
if (foundKeys.length === 0) {
  console.error('✗ 在 target/ 下没找到任何更新产物（*.sig）。先构建带更新签名的产物：');
  console.error('    macOS:   bun run build:mac --config apps/desktop/tauri.updater.conf.json');
  console.error('    Windows: bun run build:win --config apps/desktop/tauri.updater.conf.json   （需先设 TAURI_SIGNING_PRIVATE_KEY）');
  console.error('    Linux:   bun run build:linux --config apps/desktop/tauri.updater.conf.json   （需先设 TAURI_SIGNING_PRIVATE_KEY）');
  process.exit(1);
}

// ── 合并进既有 latest.json（同版本时保留其它平台的真实条目，丢弃占位模板条目）。 ──
const manifest = { version, notes, pub_date: new Date().toISOString(), platforms: {} };
if (existsSync(out)) {
  try {
    const prev = JSON.parse(readFileSync(out, 'utf8'));
    // 只在版本一致时保留既有平台条目：每个条目的 url 里都带版本号，跨版本保留会让旧平台
    // 指向上一版的下载链（首发某个新版本时尤其危险——如 0.1.14 里残留 0.1.13 的 darwin
    // 条目）。版本不同则视为新版本，从空清单开始，只写本机本次构建出的平台。
    if (prev.version === version) {
      for (const [k, v] of Object.entries(prev.platforms || {})) {
        const placeholder = !v?.signature || v.signature.includes('<<') || String(v.url).includes('REPLACE-WITH');
        if (!placeholder) manifest.platforms[k] = v;
      }
    } else if (prev.version) {
      console.warn(`  ! 既有 latest.json 版本 ${prev.version} ≠ 本次 ${version}，丢弃旧平台条目，重建清单。`);
    }
  } catch {
    console.warn(`  ! 既有 ${rel(out)} 解析失败，将重新生成。`);
  }
}
for (const key of foundKeys) {
  manifest.platforms[key] = { signature: collected[key].signature, url: collected[key].url };
}

mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, JSON.stringify(manifest, null, 2) + '\n');

if (collect) {
  mkdirSync(distDir, { recursive: true });
  for (const f of uploads) copyFileSync(f, join(distDir, basename(f)));
  writeFileSync(join(distDir, 'latest.json'), JSON.stringify(manifest, null, 2) + '\n');
}

// ── 汇报 ────────────────────────────────────────────────────────────────────
const line = '━'.repeat(66);
console.log(line);
console.log(`✓ latest.json 已写入: ${rel(out)}`);
console.log(`  版本: ${version}    仓库: ${repo}`);
console.log('  平台条目:');
for (const key of ALL_KEYS) {
  const here = foundKeys.includes(key);
  const mark = here ? '✓ 本次填入' : manifest.platforms[key] ? '· 沿用既有' : '✗ 缺失（需在对应平台构建机补齐）';
  console.log(`    ${key.padEnd(16)} ${mark}`);
}
console.log('');
console.log(`  待上传到 GitHub Release（tag v${version}）的本机产物:`);
for (const f of uploads) console.log(`    ${rel(f)}`);
console.log(`    ${rel(out)}`);
if (collect) console.log(`  已拷贝到: ${rel(distDir)}/`);
console.log(line);
