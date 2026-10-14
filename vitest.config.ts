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
    //
    // ⚠️ minWorkers 必须与 maxWorkers 一起给。vitest 2.1.9 的
    // createForksPool（node_modules/vitest/dist/chunks/resolveConfig.*.js）里：
    //   maxThreads = poolOptions.maxForks ?? config.maxWorkers ?? threadsCount
    //   minThreads = poolOptions.minForks ?? config.minWorkers ?? threadsCount
    // 只设 maxWorkers:1 时，minThreads 回落到 CPU 数（Node 24 的
    // os.availableParallelism()，本机为 8），于是 tinypool 收到
    // minThreads=8 > maxThreads=1，直接抛
    // `RangeError: options.minThreads and options.maxThreads must not conflict`，
    // 表现为「一个测试都不跑、12 秒退出」。补上 minWorkers:1 即消解。
    maxWorkers: 1,
    minWorkers: 1,
    coverage: {
      reporter: ["text", "lcov"],
    },
  },
});
