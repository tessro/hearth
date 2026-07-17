import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'node:path'

// https://vite.dev/config/
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "")

  return {
    plugins: [react(), tailwindcss()],
    resolve: {
      alias: {
        '@': path.resolve(__dirname, './src'),
      },
    },
    server: {
      host: '127.0.0.1',
      allowedHosts: ['hearth', 'hearth.home.rosania.org'],
      proxy: {
        '/v1': env.VITE_HEARTH_DEV_PROXY_TARGET || 'http://127.0.0.1:8787',
      },
    },
  }
})
