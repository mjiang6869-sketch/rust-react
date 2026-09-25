import { contextBridge, ipcRenderer } from "electron";

contextBridge.exposeInMainWorld("rustCryptoDesktop", {
  engineHealth: () => ipcRenderer.invoke("engine-health"),
});
