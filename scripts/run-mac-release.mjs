#!/usr/bin/env bun
/**
 * run-mac-release -- launcher for the one-click macOS release script.
 *
 * Mirrors run-win-release.mjs: keep package.json portable while the real
 * release workflow lives in a platform-native shell script. All argv
 * (`-DryRun`, `-NoPush`, `-SkipPull`) is forwarded to the script. A lone `--`
 * separator (some runners inject one) is stripped so switches survive.
 */
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'darwin') {
  console.error('release:mac 只能在 macOS 上运行。Windows 包用 release:win，Linux 包用 build:linux。');
  process.exit(1);
}

const forwarded = process.argv.slice(2).filter((a) => a !== '--');
const scriptPath = fileURLToPath(new URL('./release-mac.sh', import.meta.url));
const result = spawnSync('bash', [scriptPath, ...forwarded], { stdio: 'inherit' });

if (result.error) {
  console.error('启动 bash 失败:', result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
