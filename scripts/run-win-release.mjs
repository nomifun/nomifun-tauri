#!/usr/bin/env bun
/**
 * run-win-release -- launcher for the one-click Windows release script.
 *
 * Mirrors run-win-build.mjs: `release-win.ps1` is PowerShell, so invoke it
 * through whichever PowerShell is installed — PowerShell 7+ (`pwsh`) preferred
 * for correct UTF-8 console handling, falling back to the always-present
 * Windows PowerShell 5.1 (`powershell.exe`). `-ExecutionPolicy Bypass` is
 * required for the 5.1 fallback to run a local .ps1.
 *
 * All argv (`-DryRun`, `-NoPush`, `-SkipPull`) is forwarded to the script. A
 * lone `--` separator (some runners inject one) is stripped so switches survive.
 */
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'win32') {
  console.error('release:win 只能在 Windows 上运行。macOS 包用 build:mac，Linux 包用 build:linux。');
  process.exit(1);
}

/** First PowerShell that launches successfully: pwsh (7+) preferred, else powershell (5.1). */
function resolveShell() {
  for (const exe of ['pwsh', 'powershell']) {
    const probe = spawnSync(exe, ['-NoProfile', '-Command', '$PSVersionTable.PSVersion.Major'], { stdio: 'ignore' });
    if (!probe.error && probe.status === 0) return exe;
  }
  return null;
}

const shell = resolveShell();
if (!shell) {
  console.error('未找到 PowerShell（pwsh 或 powershell.exe）。请确认 Windows PowerShell 可用后重试。');
  process.exit(1);
}

const forwarded = process.argv.slice(2).filter((a) => a !== '--');
const scriptPath = fileURLToPath(new URL('./release-win.ps1', import.meta.url));
const psArgs = ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', scriptPath, ...forwarded];

const result = spawnSync(shell, psArgs, { stdio: 'inherit' });
if (result.error) {
  console.error(`启动 ${shell} 失败:`, result.error.message);
  process.exit(1);
}
process.exit(result.status ?? 1);
