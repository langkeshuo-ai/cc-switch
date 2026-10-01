import path from "node:path";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    include: ["{src,tests}/**/*.{test,spec}.{ts,tsx,js,jsx}"],
    environment: "jsdom",
    setupFiles: ["./tests/setupGlobals.ts", "./tests/setupTests.ts"],
    globals: true,
    // 慢盘环境下 jsdom 环境创建 + setup 各需 5s+，默认 5s 超时不足
    testTimeout: 30000,
    hookTimeout: 30000,
    // 并发 worker 会加剧 IO 争抢导致超时雪崩，限制为串行
    maxWorkers: 1,
    coverage: {
      reporter: ["text", "lcov"],
    },
  },
});
