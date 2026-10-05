import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  root: 'mdcheck',
  base: './',
  plugins: [react()],
  build: { outDir: 'mdcheck-dist', emptyOutDir: true },
  server: { port: 5312, strictPort: true },
});
