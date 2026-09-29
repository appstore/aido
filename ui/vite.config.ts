import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Dev loop: start the server with a pinned token and a known port,
//   AIDO_UI_TOKEN=dev aido ui --port 8710 --no-open
// then `npm run dev` — /api is proxied with changeOrigin (the Host
// check sees the aido origin, not vite's), and the token comes from
// .env.development's VITE_AIDO_TOKEN.
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': { target: 'http://127.0.0.1:8710', changeOrigin: true },
    },
  },
});
