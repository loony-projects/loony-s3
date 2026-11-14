import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';

// The dev server's address comes from frontend/.env.local, like every other setting. See
// frontend/.env.example.
function required(env: Record<string, string>, name: string): string {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${name} isn't set. Add it to frontend/.env.local (see frontend/.env.example).`);
  return value;
}

function port(env: Record<string, string>, name: string): number {
  const value = Number(required(env, name));
  if (!Number.isInteger(value) || value < 1 || value > 65535) throw new Error(`${name} must be a port number.`);
  return value;
}

export default defineConfig(({ command, mode, isPreview }) => {
  // '' loads every variable, not just VITE_ ones. Only VITE_ ones reach the browser.
  const env = loadEnv(mode, __dirname, '');
  // Building doesn't start a server, so it doesn't need an address.
  const serving = command === 'serve' || isPreview === true;
  const server = serving
    ? { host: required(env, 'DEV_HOST'), port: port(env, 'DEV_PORT'), strictPort: true }
    : undefined;

  return {
    plugins: [react()],
    resolve: {
      alias: { '@': path.resolve(__dirname, 'src') },
    },
    // No dev proxy: requests are SigV4-signed against an absolute VITE_API_URL (the
    // Host header is part of what's signed, so proxying would have to preserve it
    // transparently to work at all). A proxy is also a poor fit here regardless --
    // LS3's path-style routing (/{bucket}/{key...}) would collide with this SPA's own
    // client-side routes for any bucket named e.g. "login".
    server,
    preview: server,
  };
});
