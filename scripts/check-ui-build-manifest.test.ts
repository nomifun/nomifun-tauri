import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createUiBuildManifest } from './ui-build-manifest';

const root = join(import.meta.dir, '..');

describe('production UI build manifest', () => {
  test('identifies the app, API contract, and exact frontend build', () => {
    const uiPackage = JSON.parse(readFileSync(join(root, 'ui', 'package.json'), 'utf8')) as { version: string };
    const contractSource = readFileSync(join(root, 'ui-api-contract-version.txt'), 'utf8');
    const buildId = '12345678-1234-4123-8123-123456789abc';
    const manifest = createUiBuildManifest(uiPackage.version, contractSource, () => buildId);

    expect(manifest.schema).toBe(1);
    expect(manifest.app_version).toBe(uiPackage.version);
    expect(manifest.api_contract_version).toBe(Number.parseInt(contractSource.trim(), 10));
    expect(manifest.frontend_build_id).toBe(buildId);
    expect(Object.keys(manifest).sort()).toEqual([
      'api_contract_version',
      'app_version',
      'frontend_build_id',
      'schema',
    ]);
  });

  test('mints a UUID v4 identity for every real build', () => {
    const manifest = createUiBuildManifest('0.2.20', '1');

    expect(manifest.frontend_build_id).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
    );
  });

  test('rejects invalid contract versions and blank identities', () => {
    expect(() => createUiBuildManifest('0.2.20', '0')).toThrow();
    expect(() => createUiBuildManifest('0.2.20', 'not-a-number')).toThrow();
    expect(() => createUiBuildManifest('   ', '1')).toThrow();
    expect(() => createUiBuildManifest('0.2.20', '1', () => '   ')).toThrow();
  });

  test('web development cannot accidentally serve a stale production bundle', () => {
    const rootPackage = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as {
      scripts: Record<string, string>;
    };

    expect(rootPackage.scripts['dev:web']).toContain('--api-only');
  });

  test('production-style web serving rebuilds and pins the distribution directory', () => {
    const rootPackage = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as {
      scripts: Record<string, string>;
    };
    const command = rootPackage.scripts['serve:web'];

    expect(command.indexOf('bun run build:ui')).toBeGreaterThanOrEqual(0);
    expect(command.indexOf('bun run build:ui')).toBeLessThan(command.indexOf('cargo run -p nomifun-web'));
    expect(command).toContain('--features static-webui');
    expect(command).toContain('--dist ui/dist');
  });

  test('workspace test entry points create the desktop build placeholder on a clean clone', () => {
    const rootPackage = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8')) as {
      scripts: Record<string, string>;
    };

    for (const scriptName of ['test', 'test:fast']) {
      const command = rootPackage.scripts[scriptName];
      const ensure = command.indexOf('bun scripts/ensure-ui-dist.mjs');
      const cargo = command.indexOf('cargo ');
      expect(ensure).toBeGreaterThanOrEqual(0);
      expect(cargo).toBeGreaterThan(ensure);
    }
  });

  test('the Docker backend build is paired with the UI-stage manifest', () => {
    const dockerfile = readFileSync(join(root, 'Dockerfile'), 'utf8');

    expect(dockerfile).toContain(
      'COPY --from=ui /app/ui/dist/nomifun-build.json /src/ui/dist/nomifun-build.json'
    );
  });

  test('the Docker install path stays resilient on constrained networks', () => {
    const dockerfile = readFileSync(join(root, 'Dockerfile'), 'utf8');
    const compose = readFileSync(join(root, 'docker-compose.yml'), 'utf8');

    expect(dockerfile).toContain('ARG RUST_IMAGE="rust:1-slim-bookworm"');
    expect(dockerfile).toContain('FROM ${RUST_IMAGE} AS rust');
    expect(dockerfile).toContain('ARG BUN_REGISTRY=""');
    expect(dockerfile).toContain('Acquire::Retries=5');
    expect(dockerfile).toContain('CARGO_NET_RETRY=10');
    expect(dockerfile).toContain('CARGO_HTTP_TIMEOUT=600');
    expect(dockerfile).toContain('zlib1g-dev liblzma-dev');

    for (const buildArg of [
      'BUN_IMAGE',
      'RUST_IMAGE',
      'RUNTIME_IMAGE',
      'BUN_REGISTRY',
      'APT_MIRROR',
      'CARGO_REGISTRY_MIRROR',
    ]) {
      expect(compose).toContain(`${buildArg}: \${${buildArg}:-`);
    }
  });
});
