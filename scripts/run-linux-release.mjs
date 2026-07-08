#!/usr/bin/env bun
/**
 * run-linux-release -- launcher for the one-click Linux release script.
 *
 * Mirrors run-mac-release.mjs and run-win-release.mjs: keep package.json
 * portable while the real release workflow lives in a platform-native shell
 * script. All argv (`-DryRun`, `-NoPush`, `-SkipPull`, `--target x64`) is
 * forwarded to the script. A lone `--` separator is stripped so switches
 * survive runners that inject it.
 */
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'linux') {
  console.error('release:linux 只能在 Linux 上运行。macOS 包用 release:mac，Windows 包用 release:win。');
  process.exit(1);
}

const forwarded = process.argv.slice(2).filter((a) => a !== '--');
const scriptPath = fileURLToPath(new URL('./release-linux.sh', import.meta.url));
const result = spawnSync('bash', [scriptPath, ...forwarded], { stdio: 'inherit' });

if (result.error) {
  console.error('启动 bash 失败:', result.error.message);
  process.exit(1);
}

process.exit(result.status ?? 1);
