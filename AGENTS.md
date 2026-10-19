# 仓库指南（agent 接手入口）

本仓库是 `farion1231/cc-switch` 的个人 fork（`langkeshuo-ai/cc-switch`），Tauri 2 + Rust + React，
管理 Claude Code / Codex / Pi 的供应商切换与本地代理路由。fork 留存价值：
key 池八道闸 failover、用量统计、快照回滚、云同步、三平台自动更新。

## 开工前必读

**先读 `docs/handoff.md`**——仓库现状、待办、新机器环境、凭据迁移、坑位清单全在里面。
其他关键文档：

- `docs/adr/001-fork-charter.md`——fork 章程 + 月度上游 cherry-pick 策略（同步只做 cherry-pick，不 wholesale merge）
- `docs/fork-patches.md` / `docs/ssot.md`——fork 补丁清单 / 事实源约定
- `docs/compare-magpie-20261010.md`——与 magpie 的边界（可共存，勿把 magpie 功能搬进来重复造轮子）

## 闸门（交付前必须全绿）

```bash
cd src-tauri && cargo fmt --all --check && cargo clippy --lib && cargo test --lib -- --test-threads=1
cd .. && pnpm typecheck && pnpm format:check && pnpm test:unit   # 全量约 11-13 分钟，后台跑
```

- `cargo test` 必须 `--test-threads=1`（测试有共享状态）。
- 测试基建：Tauri invoke 走 MSW 打桩（`tests/msw/`，端点 `http://tauri.local/<command>`）；
  组件测试 mock react-i18next 为 `t: key => key`。
- 新功能：决策逻辑先抽纯函数配单测，交互路径写组件测试
  （参照 `src/components/universal/unifiedRows.ts` 与 `tests/components/AppRoutingPanel.test.tsx`）。

## 工作铁律（用户硬性约束）

- 不保留向后兼容，过时直接删；最简实现，不做预防性抽象。
- 用户可见输出 / 注释 / commit message 一律简体中文。
- 变更边界：每处改动直接追溯当前需求，禁止顺手改相邻代码 / 格式 / 命名。
- commit 格式：`类型(范围): 简短中文标题` + 空行 + `-` 开头中文要点 + Co-Authored-By trailer。
- 无验证不交付：完成声明必须附命令 + 真实输出 + exit code；修 bug 先写复现测试。
- 禁止交互性测试（GUI 冒烟由用户做）；严禁硬编码凭据；未授权不 commit / push / reset / checkout --。
- 完整铁律见 `docs/handoff.md` §9。
