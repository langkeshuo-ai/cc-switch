# BASELINE.md — 裁剪基线与边界声明

> 本文件是 fork 维护模型的权威基线记录。修改裁剪规则前必须先更新本文件。

## 1. 上游基线

| 项 | 值 | 核对方式 |
|---|---|---|
| 上游仓库 | `farion1231/cc-switch` | git remote `upstream` |
| 基线上游 tag | **v3.20.4** | merge-base `1ee2fdc3`（上游 docs refresh for v3.20.4），其 `package.json` version = `3.20.4` |
| 本 fork 版本 | `3.20.4-trim.13` | `package.json` / `src-tauri/tauri.conf.json` / `src-tauri/Cargo.toml` 三处**一致**（2026-10-10 交叉核对通过） |
| 上游最新 tag（参考） | v4.0.6（2026-10-09 发布）；4.0.x 的 `SCHEMA_VERSION = 20` | 升级决策见 T5 评估报告，未定前基线保持 v3.20.4 |

**版本核对结果**：三处版本号一致，无漂移。

## 2. 数据库 schema 不变量（最高优先级）

> **本 fork 的 `SCHEMA_VERSION` 必须等于基线上游（v3.20.4）的值：`19`。**
> 上游 4.0.x 为 `20`（其 v19→v20 迁移内容是给 `mcp_servers` 加 `enabled_pi`）。

- 禁止任何 fork 自造迁移；fork 曾自造的三条迁移（v19→v20 `app_snapshots` 建表、v20→v21 放行 Pi proxy_config 行、v21→v22 清理被裁应用）已全部删除。
- `proxy_config` 的 CHECK 约束保持上游四值：`('claude','codex','gemini','grokbuild')`，**不含 `'pi'`**。
- 存量 fork 数据库（v20/v21/v22）由启动时的 `normalize_legacy_trim_schema` 自动降级归一到上游 v19 形状，**无需手工改库**。
- 理由：fork 曾把 schema 顶到 22，导致每次跟上游都要手改数据库、且与官方构建互不兼容（官方装在 fork 库上直接拒绝启动）。schema 对齐后，跟版只 diff 代码，不动库。

## 3. 裁剪范围

只保留 **Claude Code / Codex CLI / Pi coding agent** 三个应用。

**删除的 7 个应用**及其对应 Rust 模块（对照上游 v3.20.4）：

| 应用 | 已删除路径 |
|---|---|
| claude-desktop | `src-tauri/src/claude_desktop_config.rs`（用量展示中保留 `claude-desktop` 标识符，折入 claude 口径） |
| gemini | `src-tauri/src/gemini_config.rs`、`gemini_mcp.rs`（`gemini_shadow` 基础设施保留：claude 供应商的 gemini_native 格式仍依赖） |
| grok / grokbuild | `src-tauri/src/grok_config.rs` |
| opencode | `src-tauri/src/opencode_config.rs` |
| openclaw | `src-tauri/src/openclaw_config.rs` |
| hermes | `src-tauri/src/hermes_config.rs` |
| mcode | `src-tauri/src/mcode_config.rs` |

前端同步收窄：`AppId` 限 `claude | codex | pi`，预设、表单、组件与测试随删。
`mcp/`、`proxy/`、`services/`、`session_manager/`、`commands/` 为部分删除（按应用域裁剪）。

## 4. 边界声明

> 本 fork 仅做减法。Pi 的本地路由不在本仓库实现——它通过
> `~/.pi/agent/models.json` 中的固定节点接入官方网关实现。
> 详见 `docs/PI-ROUTING.md`。

具体含义（硬性约束，违反即视为失败）：

- ❌ 不得引入任何新的数据库迁移，不得让 `SCHEMA_VERSION` 高于同版本上游的值
- ❌ 不得把 `'pi'` 加回 `proxy_config` 的 CHECK 约束或任何 `PROXY_APPS` 等价物
- ❌ 不得新增 Pi 的网关 / 路由 / 故障转移 / MCP / 插件 / 主题支持
- ❌ 不得手写代码去删上游文件来"裁剪"（必须脚本化，见 `scripts/trim-app.mjs` + `scripts/trim-rules.json`）
- ❌ 不得关闭测试来让 CI 变绿
- ✅ 可以：减少应用数量、减少预设、精简 UI、加快构建

## 5. Pi 功能边界（与代码核对过的一致状态）

README 功能表 Pi 行（中/英/日三份）已逐列核对代码：

| 列 | 值 | 代码依据 |
|---|---|---|
| 供应商 | 共存 | `pi_config` 管理 `models.json` 显式供应商条目（上游 #6064 设计） |
| 本地路由 | – | fork 自造的 `/pi/*` 代理层已删除（T3） |
| 托盘切换 | – | `tray.rs` 仅硬编码 Claude/Codex 两个 section |
| MCP | – | `app_config.rs`：Pi core 无原生 MCP 注册表 |
| Skills | ✓ | `services/skill.rs` 含 Pi 部署逻辑 |
| 会话 / 用量统计 | ✓ | `session_manager/providers/pi.rs` 等 |

## 6. 版本号规则（`-trim.N` 递增条件）

- **递增**：合并一批上游修复/功能（月度 ADR-001 pick 评审）并重新打包发布时；或 fork 自身发生行为变更的修复/重构后发布时。
- **不递增**：纯文档、注释、CI 配置修正（合入下一批次一起发）。
- 命名：`v{上游基线版本}-trim.{N}`，如 `v3.20.4-trim.14`。升上游基线（如 4.0）后 N 归零重计：`v4.0.x-trim.1`。

## 7. 跟版流程

1. `git fetch upstream --tags`
2. 若上游 `SCHEMA_VERSION` 变化：先跑 T5 式只读评估，再决定是否跟。
3. 跑 `node scripts/trim-app.mjs <上游tag>` 产出裁剪补丁（T4），按补丁 cherry-pick/合并。
4. 全量闸门：typecheck + cargo check + cargo test --lib + 打包验证。
5. 递增 `-trim.N`，发布。
