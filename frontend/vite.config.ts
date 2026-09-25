import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    port: 5174,
    strictPort: true,
    proxy: {
      // Rust API 与 WebSocket 都代理到本地服务。
      // 用代理而非 CORS：前端只需知道同源路径，部署形态变化时不必改代码。
      '/api': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: false,
        ws: true,
      },
    },
  },
  build: {
    // 相对路径：让构建产物可以从任意子路径提供服务，
    // 也便于以后用 file:// 打开（Electron 打包场景）。
    base: './',
    outDir: 'dist',
    sourcemap: true,
  },
})
