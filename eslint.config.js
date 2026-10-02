// ESLint flat config —— 最小有效集。
//
// 为什么只开这几条：本仓库 154 个 useEffect 依赖数组此前**无任何自动化守护**，
// `exhaustive-deps` 缺失导致的 stale closure 是本类桌面应用最隐蔽的 bug 来源
// （表现为"UI 显示已切换，实际请求仍打旧端点"）。但全量规则（react/recommended
// + jsx-a11y + no-unused-vars…）会对存量代码刷出数百条历史告警，让人直接
// 关掉整条流水线——那等于没加。
//
// 策略：**只开与本项目实际风险相关的规则，且 warn 级（不阻断）**。
// CI 用 `eslint .` 记录现状，新代码不再恶化；存量债另行专项治理。
//
// 引入时不要顺手加 formatting 规则：prettier 已单独在 CI 跑 `format:check`，
// 两者同时管同一件事必然打架。

import js from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";

export default tseslint.config(
  {
    ignores: [
      "dist/**",
      "node_modules/**",
      "src-tauri/**",
      "coverage/**",
      "*.config.{js,ts,mjs,cjs}",
      "scripts/**",
      "tests/msw/**",
    ],
  },

  // ── TypeScript 源码 ──
  {
    files: ["src/**/*.{ts,tsx}"],
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    languageOptions: {
      ecmaVersion: 2022,
      globals: { ...globals.browser, ...globals.es2021 },
    },
    plugins: { "react-hooks": reactHooks },
    rules: {
      // 本项目的核心守护点：hook 依赖缺失 = stale closure
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",

      // TS 已由 tsc 严格检查，重复的 lint 规则只增噪音
      "@typescript-eslint/no-explicit-any": "warn",
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { argsIgnorePattern: "^_", varsIgnorePattern: "^_" },
      ],
      // 空 catch 会吞掉真实错误（本仓库多处 error 旁路钩子是有意为之，
      // 但大多应显式注释说明），先 warn 不阻断
      "no-empty": ["warn", { allowEmptyCatch: true }],
    },
  },
);
