# Electron 桌面端

Electron main 进程启动 Rust engine sidecar 和 frontend Vite renderer。开发环境只需要先编译 Rust，再在本目录安装 Electron 依赖并运行：

```sh
pnpm install
cargo build
pnpm dev
```

生产打包前需要把 Rust release binary 放到 `RUST_CRYPTO_ENGINE` 指定的位置，并把 renderer 改为打包后的资源协议。当前外壳只暴露健康检查，交易、凭据和订单能力必须继续由 Rust API 负责。

打包运行时，Electron 会加载 `frontend/dist/index.html`，并默认寻找 `process.resourcesPath/rust-crypto-engine`。发布流程需要先执行 `cd frontend && pnpm build`、`cargo build --release`，再把 release binary 复制为该 sidecar 文件；也可以用 `RUST_CRYPTO_ENGINE` 覆盖路径。开发模式仍由 Electron 自动启动 Vite。
