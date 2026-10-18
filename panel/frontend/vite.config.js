import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `npm run dev` proxies the API to a panel running on localhost:8080.
const target = process.env.PANEL_URL || 'http://127.0.0.1:8080';

export default defineConfig({
  plugins: [svelte()],
  server: {
    proxy: {
      '/api': target,
      '/ws': { target: target.replace(/^http/, 'ws'), ws: true },
    },
  },
});
