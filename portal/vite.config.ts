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
    },
  },
})
