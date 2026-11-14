import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react';
import fs from 'fs';
import path from 'path';

// Every setting comes from the scripts' settings file, as for the server: scripts/.env, or
// the file named by LS3_ENV_FILE. See scripts/.env.example and docs/configuration.md.
const repoRoot = path.resolve(__dirname, '..');
const mainFile = path.join(repoRoot, 'scripts/.env');

/** The same rules as scripts/env.sh: KEY=VALUE, no inline comments, and one matching pair
 * of surrounding quotes stripped. */
function parseEnvFile(file: string): Record<string, string> {
  const values: Record<string, string> = {};
  for (const raw of fs.readFileSync(file, 'utf8').split('\n')) {
    const match = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(raw.replace(/\r$/, ''));
    if (!match) continue;
    const quoted = /^"(.*)"$/.exec(match[2]) ?? /^'(.*)'$/.exec(match[2]);
    values[match[1]] = quoted ? quoted[1] : match[2];
  }
  return values;
}

function loadSettings(): Record<string, string> {
  const chosen = process.env.LS3_ENV_FILE ? path.resolve(repoRoot, process.env.LS3_ENV_FILE) : mainFile;
  if (!fs.existsSync(chosen)) throw new Error(`${chosen} doesn't exist. Create it from scripts/.env.example.`);
  const values = parseEnvFile(chosen);
  // A second instance shares only sign-in with the main file, so the client secret lives in
  // one place and a dev copy can never pick up the live server's ports or data.
  if (chosen !== mainFile && fs.existsSync(mainFile)) {
    for (const [key, value] of Object.entries(parseEnvFile(mainFile))) {
      if (key.startsWith('LS3_OIDC_') && !values[key]) values[key] = value;
    }
  }
  // As in the scripts, what's already set in the environment wins.
  for (const key of Object.keys(values)) {
    const fromShell = process.env[key];
    if (fromShell) values[key] = fromShell;
  }
  return values;
}

function required(settings: Record<string, string>, name: string): string {
  const value = settings[name]?.trim();
  if (!value) throw new Error(`${name} isn't set. Add it to the scripts' settings file (see scripts/.env.example).`);
  return value;
}

function address(value: string, name: string): { host: string; port: number } {
  const match = /^(.+):(\d+)$/.exec(value);
  const port = match ? Number(match[2]) : NaN;
  if (!match || !Number.isInteger(port) || port < 1 || port > 65535) throw new Error(`${name} must be host:port.`);
  return { host: match[1].replace(/^\[(.*)\]$/, '$1'), port };
}

/** Serves what loony-server would serve at /config.json, from the same settings. */
function serverSettings(settings: Record<string, string>): Plugin {
  const body = JSON.stringify({
    api_url: required(settings, 'LS3_PUBLIC_API_URL').replace(/\/$/, ''),
    region: required(settings, 'REGION'),
    admin_api_url: `http://${required(settings, 'LS3_ADMIN_ADDR')}`,
    oidc_issuer: settings.LS3_OIDC_ISSUER?.trim() || null,
    oidc_client_id: settings.LS3_OIDC_CLIENT_ID?.trim() || null,
  });
  const serve = (server: { middlewares: { use: (path: string, handler: (req: unknown, res: import('http').ServerResponse) => void) => void } }) => {
    server.middlewares.use('/config.json', (_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.setHeader('Cache-Control', 'no-cache');
      res.end(body);
    });
  };
  return { name: 'ls3-server-settings', configureServer: serve, configurePreviewServer: serve };
}

export default defineConfig(({ command, isPreview }) => {
  const plugins: Plugin[] = [react()];
  let server;
  // A build needs no settings: the page asks the server for them when it starts.
  if (command === 'serve' || isPreview === true) {
    const settings = loadSettings();
    plugins.push(serverSettings(settings));
    server = { ...address(required(settings, 'LS3_UI_ADDR'), 'LS3_UI_ADDR'), strictPort: true };
  }

  return {
    plugins,
    resolve: {
      alias: { '@': path.resolve(__dirname, 'src') },
    },
    // No dev proxy: requests are SigV4-signed against an absolute API address (the
    // Host header is part of what's signed, so proxying would have to preserve it
    // transparently to work at all). A proxy is also a poor fit here regardless --
    // LS3's path-style routing (/{bucket}/{key...}) would collide with this SPA's own
    // client-side routes for any bucket named e.g. "login".
    server,
    preview: server,
  };
});
