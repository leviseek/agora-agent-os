import { defineConfig, loadEnv } from 'vite';
import react from '@vitejs/plugin-react';

/** Where the runtime gateway listens by default (see agentos-core config: api.http_addr). */
const DEFAULT_RUNTIME_TARGET = 'http://127.0.0.1:8788';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, '.', '');
  const target = (env.AGENTOS_HTTP_TARGET ?? '').trim() || DEFAULT_RUNTIME_TARGET;

  return {
    plugins: [react()],
    server: {
      port: 5173,
      strictPort: true,
      proxy: {
        // Same-origin API access: the browser talks to :5173, Vite forwards to the runtime.
        '/healthz': { target, changeOrigin: true },
        '/readyz': { target, changeOrigin: true },
        '/v1': { target, changeOrigin: true, ws: true },
      },
    },
    build: {
      outDir: 'dist',
      sourcemap: true,
    },
  };
});
