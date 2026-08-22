import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Dev-time proxy to a locally running router; production serves the built
// `dist/` directory from the router itself (same origin, no proxy).
const backend = 'http://localhost:8080'

export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist' },
  server: {
    proxy: {
      '/api': backend,
      '/auth': backend,
      '/webhooks': backend,
      '/.well-known': backend,
      // The inference plane. Two pages read it — the storefront's catalog and
      // the playground, which posts its completions here — and neither works
      // under `vite dev` without this. It was missing while only `/v1/models`
      // needed it, so the dev server rendered an empty catalog and nobody
      // noticed; the playground would have failed more loudly.
      '/v1': backend,
    },
  },
})
