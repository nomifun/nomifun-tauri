import { defineConfig, type Plugin } from 'vite';
import { readFileSync } from 'node:fs';
import { resolve } from 'path';
import react from '@vitejs/plugin-react';
import UnoCSS from 'unocss/vite';
import unoConfig from './uno.config.ts';
import { createUiBuildManifest } from '../scripts/ui-build-manifest';

const uiPackage = JSON.parse(readFileSync(resolve(__dirname, 'package.json'), 'utf8')) as { version: string };

function uiBuildManifestPlugin(): Plugin {
  const rawApiContractVersion = readFileSync(resolve(__dirname, '../ui-api-contract-version.txt'), 'utf8').trim();
  const manifest = createUiBuildManifest(uiPackage.version, rawApiContractVersion);

  return {
    name: 'nomifun-ui-build-manifest',
    apply: 'build' as const,
    generateBundle() {
      this.emitFile({
        type: 'asset',
        fileName: 'nomifun-build.json',
        source: `${JSON.stringify(manifest, null, 2)}\n`,
      });
    },
  };
}

// Ported from the original electron.vite.config.ts: rewrites named imports from
// '@icon-park/react' into HOC-wrapped components (replaces the old webpack loader).
function iconParkPlugin() {
  return {
    name: 'vite-plugin-icon-park',
    enforce: 'pre' as const,
    transform(source: string, id: string) {
      if (!id.endsWith('.tsx') || id.includes('node_modules')) return null;
      if (!source.includes('@icon-park/react')) return null;
      const transformed = source.replace(
        /import\s+\{\s+([a-zA-Z, ]*)\s+\}\s+from\s+['"]@icon-park\/react['"](;?)/g,
        (str, match) => {
          if (!match) return str;
          const components = match.split(',');
          const importComponent = str.replace(match, components.map((k: string) => `${k} as _${k.trim()}`).join(', '));
          const hoc = `import IconParkHOC from '@renderer/components/IconParkHOC';
${components.map((k: string) => `const ${k.trim()} = IconParkHOC(_${k.trim()})`).join(';\n')}`;
          return importComponent + ';' + hoc;
        }
      );
      return transformed !== source ? { code: transformed, map: null } : null;
    },
  };
}

const src = resolve(__dirname, 'src');

// NOTE (P1): the renderer/common reaches `electron` in 6 sites under
// src/common/adapter and src/common/platform. The Tauri shim (P1) replaces
// those with a Tauri-invoke transport + a `window.__backendPort` init script.
// Until then a browser/Tauri build of those paths will fail to resolve 'electron'.
export default defineConfig(({ mode }) => {
  // WebUI dev mode (`vite --mode webdev`, driven by the UI `dev:web` script and
  // the root `dev:webui` one-click). The SPA is served by Vite (with HMR) but the
  // auth backend + API + WebSocket live in the separate `nomifun-web` host. The
  // browser SPA makes *same-origin relative* calls in WebUI mode (`/api/*`,
  // `/login`, `/logout`, `/ws` — see ui/src/common/adapter/httpBridge.ts), so we
  // proxy that backend surface to nomifun-web. Without this, those calls hit the
  // static dev server and fail at the network layer ("连接失败").
  //
  // Gated on `mode === 'webdev'` so plain `ui:dev` and the Tauri desktop dev
  // server (mode 'development', which talks to the embedded backend via an
  // absolute `window.__backendPort` URL) are completely unaffected.
  const webdev = mode === 'webdev';
  const apiPort = process.env.NOMIFUN_WEB_PORT ?? '8787';
  const apiTarget = `http://127.0.0.1:${apiPort}`;
  const wsTarget = `ws://127.0.0.1:${apiPort}`;
  // Same loopback host on both sides — keep the original Host header so the
  // backend's host-only session cookie maps cleanly back onto 127.0.0.1:5173.
  const httpProxy = { target: apiTarget, changeOrigin: false };
  const proxy = webdev
    ? {
        '/api': httpProxy,
        '/login': httpProxy,
        '/logout': httpProxy,
        '/qr-login': httpProxy,
        '/health': httpProxy,
        '/ws': { target: wsTarget, ws: true, changeOrigin: false },
      }
    : undefined;

  return {
    root: __dirname,
    // Pin the dev server so it always matches the Tauri `devUrl` (5173).
    server: {
      port: 5173,
      strictPort: true,
      host: '127.0.0.1',
      proxy,
    },
    plugins: [iconParkPlugin(), react(), UnoCSS({ ...unoConfig }), uiBuildManifestPlugin()],
    resolve: {
      alias: {
        '@': src,
        '@common': resolve(src, 'common'),
        '@renderer': resolve(src, 'renderer'),
        '@xterm/headless': resolve(src, 'common/utils/shims/xterm-headless.ts'),
      },
      extensions: ['.ts', '.tsx', '.js', '.json'],
    },
    build: {
      outDir: 'dist',
      emptyOutDir: true,
      reportCompressedSize: false,
    },
  };
});
