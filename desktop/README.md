# Electron 桌面端

Electron main 进程启动 Rust engine sidecar 和 frontend Vite renderer。开发环境只需要先编译 Rust，再在本目录安装 Electron 依赖并运行：

```sh
pnpm install
cargo build
pnpm dev
```

生产打包前需要把 Rust release binary 放到 `RUST_CRYPTO_ENGINE` 指定的位置，并把 renderer 改为打包后的资源协议。当前外壳只暴露健康检查，交易、凭据和订单能力必须继续由 Rust API 负责。
