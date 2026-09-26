import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 后端地址。默认 8080，但**必须可配置**——端口被占用时用户会换端口，
// 硬编码会让前端静默失联（代理返回 500，界面上只看到「无法连接后端」，
// 而真正的原因是代理指错了地方）。
//
// 运行时通过环境变量 RUST_CRYPTO_API_URL 覆盖，由 scripts/run.sh 传入。
const apiTarget = process.env.RUST_CRYPTO_API_URL ?? 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [react()],
  // /market 等直达地址在生产构建中仍从站点根目录加载资源。
  base: '/',
  server: {
    host: '127.0.0.1',
    port: 5174,
    strictPort: true,
    proxy: {
      // Rust API 与 WebSocket 都代理到本地服务。
      // 用代理而非 CORS：前端只需知道同源路径，部署形态变化时不必改代码。
      '/api': {
        target: apiTarget,
        changeOrigin: false,
        ws: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
  },
})
