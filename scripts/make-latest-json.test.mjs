#!/usr/bin/env bun
import assert from 'node:assert/strict';
import { rmSync, mkdirSync, writeFileSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const version = readWorkspaceVersion();
const testDir = join(root, 'build.noindex', 'make-latest-json-test');
const targetDir = join(testDir, 'target');
const outPath = join(testDir, 'latest.json');
const existingNotes = `Existing release notes for ${version}`;

rmSync(testDir, { recursive: true, force: true });
mkdirSync(testDir, { recursive: true });

const windowsFixtureDir = join(targetDir, 'x86_64-test-windows-msvc', 'release', 'bundle', 'nsis');
const windowsArtifact = join(windowsFixtureDir, `NomiFun_${version}_x64-setup.exe`);
writeArtifactWithSig(windowsArtifact, 'fake windows installer', 'fake windows signature');

const linuxFixtureDir = join(targetDir, 'x86_64-unknown-linux-gnu', 'release', 'bundle');
const appImageArtifact = join(linuxFixtureDir, 'appimage', `NomiFun_${version}_amd64.AppImage`);
const debArtifact = join(linuxFixtureDir, 'deb', `NomiFun_${version}_amd64.deb`);
const rpmArtifact = join(linuxFixtureDir, 'rpm', `NomiFun-${version}-1.x86_64.rpm`);
writeArtifactWithSig(rpmArtifact, 'fake rpm', 'fake rpm signature');
writeArtifactWithSig(debArtifact, 'fake deb', 'fake deb signature');
writeArtifactWithSig(appImageArtifact, 'fake appimage', 'fake appimage signature');

writeFileSync(
  outPath,
  JSON.stringify(
    {
      version,
      notes: existingNotes,
      pub_date: '2026-07-05T00:00:00.000Z',
      platforms: {
        'darwin-x86_64': {
          signature: 'existing darwin signature',
          url: `https://github.com/example/repo/releases/download/v${version}/NomiFun.app.tar.gz`,
        },
      },
    },
    null,
    2,
  ) + '\n',
);

try {
  const scriptPath = join(root, 'scripts', 'make-latest-json.mjs');
  const result = spawnSync('bun', [scriptPath, '--out', outPath, '--repo', 'example/repo', '--target-dir', targetDir], {
    cwd: root,
    encoding: 'utf8',
  });

  assert.equal(result.status, 0, result.stderr || result.stdout);

  const manifest = JSON.parse(readFileSync(outPath, 'utf8'));
  assert.equal(manifest.version, version);
  assert.equal(manifest.notes, existingNotes);
  assert.ok(manifest.platforms['darwin-x86_64']);
  assert.ok(manifest.platforms['windows-x86_64']);
  assert.equal(
    manifest.platforms['linux-x86_64'].url,
    `https://github.com/example/repo/releases/download/v${version}/NomiFun_${version}_amd64.AppImage`,
  );
  assert.equal(manifest.platforms['linux-x86_64'].signature, 'fake appimage signature');
} finally {
  rmSync(testDir, { recursive: true, force: true });
}

function writeArtifactWithSig(path, artifactContent, signatureContent) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, artifactContent);
  writeFileSync(`${path}.sig`, signatureContent);
}

function readWorkspaceVersion() {
  const lines = readFileSync(join(root, 'Cargo.toml'), 'utf8').split('\n');
  let inSection = false;
  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed.startsWith('[')) {
      inSection = trimmed === '[workspace.package]';
      continue;
    }
    if (inSection) {
      const match = line.match(/^\s*version\s*=\s*"([^"]+)"/);
      if (match) return match[1];
    }
  }
  throw new Error('Unable to read workspace version');
}
