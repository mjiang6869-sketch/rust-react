import { app, BrowserWindow, ipcMain, shell } from "electron";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const projectRoot = join(import.meta.dirname, "..");
const packagedEngine = join(process.resourcesPath, "rust-crypto-engine");
const engineCommand = process.env.RUST_CRYPTO_ENGINE ?? (app.isPackaged
  ? packagedEngine
  : join(projectRoot, "target", "debug", "rust-crypto"));
const rendererUrl = process.env.RUST_CRYPTO_RENDERER_URL ?? "http://127.0.0.1:5174";
let engine;
let renderer;

function packagedRenderer() {
  return join(process.resourcesPath, "frontend", "dist", "index.html");
}

function startEngine() {
  if (!existsSync(engineCommand)) {
    throw new Error(`Rust engine 不存在：${engineCommand}`);
  }
  engine = spawn(engineCommand, [], {
    cwd: app.isPackaged ? process.resourcesPath : projectRoot,
    env: {
      ...process.env,
      RUST_CRYPTO_BIND: "127.0.0.1:8080",
      RUST_CRYPTO_STATE_PATH: process.env.RUST_CRYPTO_STATE_PATH
        ?? join(app.getPath("userData"), "state", "default.json"),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  engine.stdout?.on("data", (chunk) => console.log(`[rust-engine] ${chunk}`));
  engine.stderr?.on("data", (chunk) => console.error(`[rust-engine] ${chunk}`));
  engine.on("exit", (code, signal) => {
    if (!app.isPackaged) console.log(`Rust engine 已退出 code=${code} signal=${signal}`);
  });
}

function startRenderer() {
  if (app.isPackaged || process.env.RUST_CRYPTO_RENDERER_URL) return;
  const pnpm = process.env.RUST_CRYPTO_PNPM ?? "pnpm";
  renderer = spawn(pnpm, ["--dir", join(projectRoot, "frontend"), "dev", "--host", "127.0.0.1", "--port", "5174"], {
    cwd: projectRoot,
    env: process.env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  renderer.stdout?.on("data", (chunk) => console.log(`[renderer] ${chunk}`));
  renderer.stderr?.on("data", (chunk) => console.error(`[renderer] ${chunk}`));
}

async function waitForRenderer() {
  if (app.isPackaged || process.env.RUST_CRYPTO_RENDERER_URL) return;
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      const response = await fetch(rendererUrl);
      if (response.ok) return;
    } catch {
      // Vite is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error("Vite renderer 在 15 秒内没有启动");
}

function createWindow() {
  const window = new BrowserWindow({
    width: 1440,
    height: 960,
    minWidth: 1080,
    minHeight: 720,
    webPreferences: {
      preload: join(import.meta.dirname, "preload.mjs"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });
  if (app.isPackaged) {
    void window.loadFile(packagedRenderer());
  } else {
    void window.loadURL(rendererUrl);
  }
  window.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });
}

app.whenReady().then(async () => {
  if (app.isPackaged && !existsSync(packagedRenderer())) {
    throw new Error(`打包后的 renderer 不存在：${packagedRenderer()}`);
  }
  if (app.isPackaged) mkdirSync(join(app.getPath("userData"), "state"), { recursive: true });
  startEngine();
  startRenderer();
  await waitForRenderer();
  ipcMain.handle("engine-health", async () => {
    const response = await fetch("http://127.0.0.1:8080/api/health");
    return response.text();
  });
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("before-quit", () => {
  engine?.kill("SIGTERM");
  renderer?.kill("SIGTERM");
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
