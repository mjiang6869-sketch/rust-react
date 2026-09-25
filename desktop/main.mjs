import { app, BrowserWindow, ipcMain, shell } from "electron";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";

const projectRoot = join(import.meta.dirname, "..");
const engineCommand = process.env.RUST_CRYPTO_ENGINE ?? join(projectRoot, "target", "debug", "rust-crypto");
const rendererUrl = process.env.RUST_CRYPTO_RENDERER_URL ?? "http://127.0.0.1:5174";
let engine;

function startEngine() {
  if (!existsSync(engineCommand)) {
    throw new Error(`Rust engine 不存在：${engineCommand}`);
  }
  engine = spawn(engineCommand, [], {
    cwd: projectRoot,
    env: { ...process.env, RUST_CRYPTO_BIND: "127.0.0.1:8080" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  engine.stdout?.on("data", (chunk) => console.log(`[rust-engine] ${chunk}`));
  engine.stderr?.on("data", (chunk) => console.error(`[rust-engine] ${chunk}`));
  engine.on("exit", (code, signal) => {
    if (!app.isPackaged) console.log(`Rust engine 已退出 code=${code} signal=${signal}`);
  });
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
  void window.loadURL(rendererUrl);
  window.webContents.setWindowOpenHandler(({ url }) => {
    void shell.openExternal(url);
    return { action: "deny" };
  });
}

app.whenReady().then(() => {
  startEngine();
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
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
