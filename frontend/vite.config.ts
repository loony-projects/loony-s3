import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': path.resolve(__dirname, 'src') },
  },
  server: {
    port: 5173,
    // No dev proxy: requests are SigV4-signed against an absolute VITE_API_URL (the
    // Host header is part of what's signed, so proxying would have to preserve it
    // transparently to work at all). A proxy is also a poor fit here regardless --
    // LS3's path-style routing (/{bucket}/{key...}) would collide with this SPA's own
    // client-side routes for any bucket named e.g. "login".
  },
});
