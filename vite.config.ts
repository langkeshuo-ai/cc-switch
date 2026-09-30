import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { codeInspectorPlugin } from "code-inspector-plugin";

export default defineConfig(({ command, mode }) => ({
  root: "src",
  plugins: [
    command === "serve" &&
      codeInspectorPlugin({
        bundler: "vite",
      }),
    react(),
  ].filter(Boolean),
  base: "./",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
  },
  // 生产构建剥离 console.log/info/debug 与 debugger；保留 console.error/warn
  // 用于诊断。早期未配置，137 处 console.* 全部进入生产 bundle，可能在
  // WebView devtools 中泄漏内部状态（provider 名、URL、部分凭据前缀）。
  esbuild:
    mode === "production"
      ? {
          pure: ["console.log", "console.info", "console.debug"],
          drop: ["debugger"],
        }
      : undefined,
  server: {
    port: 3000,
    strictPort: true,
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_"],
}));

