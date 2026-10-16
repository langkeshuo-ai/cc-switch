# BASELINE.md — 裁剪基线与边界声明

> 本文件是 fork 维护模型的权威基线记录。修改裁剪规则前必须先更新本文件。
>
> **基线时间点：2026-10-16**。凡本文件与代码现状冲突，**以代码为准**并立即更新本文件。
> 本文曾因未标注时间点而停在 `4.0.6-trim.1` 时代的事实上，先后误导过 Coder 与 Recon 各一次。
> 核对命令：`package.json` / `src-tauri/Cargo.toml` / `src-tauri/tauri.conf.json` / `src-tauri/Cargo.lock`
> 四处 version，以及 `src-tauri/src/database/mod.rs` 的 `SCHEMA_VERSION`。

## 1. 上游基线

| 项 | 值 | 核对方式 |
|---|---|---|
| 上游仓库 | `farion1231/cc-switch` | git remote `upstream` |
| 基线上游 tag | **v4.0.6** | merge commit `06147412`（上游 v4.0.6 合入，冲突以上游为准），其 `package.json` version = `4.0.6` |
| 本 fork 版本 | **`4.0.13`** | 四处**一致**（2026-10-16 核对通过）：`package.json:3` / `src-tauri/Cargo.toml:3` / `src-tauri/tauri.conf.json:4` / `src-tauri/Cargo.lock:849`。fork 已在 `faa4ad00` 并入上游版本线号，故不再使用 `-trim.N` 后缀，后续 cherry-pick 与上游同基准 |
| 历史基线（参考） | v3.20.4（merge-base `1ee2fdc3`）→ trim.13；v4.0.6 升级后 N 归零重计；v4.0.13 起弃用 `-trim.N` | 跟版流程见 §7 |

**版本核对结果**：四处版本号一致，无漂移。（本行曾错误地声称「三处一致」——实际第四处 `Cargo.lock` 长期漂移，已由 `d3f772d1` 修复，故此处理应核对四处。）

## 2. 数据库 schema（现状记录 + 不变量状态）

> **SCHEMA_VERSION 当前值 = `22`**（`src-tauri/src/database/mod.rs:65`，2026-10-16 核对）。

以下逐条标注「是现状」还是「历史记录」，以及各条不变量的当前成立状态：

| 条 | 内容 | 状态 |
|---|---|---|
| ① | `SCHEMA_VERSION` = 22 | ✅ **现状**（代码 `database/mod.rs:65`） |
| ② | 原不变量「必须等于基线上游（v4.0.6）的值 20」 | ❌ **当前不成立**。上游 v4.0.6 之后持续推进 schema，fork 跟版到 4.0.13 后实际值已高于 20。22 是**已接受的既成事实**——任务书「当前水位」记录的正是 `schema v22：key_id + route_reason 列已在` |
| ③ | 原不变量「三条自造迁移已删除、schema 回退上游基线」 | ⚠️ **历史记录，非现状**。fork 确实删掉过自造迁移（v19→v20 `app_snapshots` 建表、v20→v21 放行 Pi proxy_config 行、v21→v22 清理被裁应用，T2），但此后 schema 又随上游推进到 22，故「已回退上游基线」这句不再描述当前状态 |
| ④ | `proxy_config` 的 CHECK 约束保持上游四值 `('claude','codex','gemini','grokbuild')`，**不含 `'pi'`** | ✅ **现状仍成立**（2026-10-16 核对：`src-tauri/src/database/schema.rs:130 / 952 / 1514` 三处均为四值） |
| ⑤ | 禁止任何 fork 自造迁移 | ✅ **仍然有效**（这是规则，不是状态描述；新增迁移一律走上游） |
| ⑥ | 存量 fork 数据库（v20/v21/v22）由启动时的版本预检放行（`093fb91d`）：高于 `SCHEMA_VERSION` 拒绝、等于/低于则正常迁移，**无需手工改库** | ✅ **机制仍成立**；版本范围已与当前 22 对齐 |

> 历史理由（保留备查）：fork 曾把 schema 顶到 22，导致每次跟上游都要手改数据库、且与官方构建互不兼容（官方装在 fork 库上直接拒绝启动）。
>
> **不变量 ② 已被代码推翻这一事实，需走单独决策确认是「接受 22 为新基线」还是「回退到上游当前值」**；本文件只如实登记，不擅自改规则。

## 3. 裁剪范围

只保留 **Claude Code / Codex CLI / Pi coding agent** 三个应用。

**删除的 7 个应用**及其对应 Rust 模块（对照上游 v4.0.6）：

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

## 6. 版本号规则

> **2026-10-16 更新：fork 已弃用 `-trim.N` 后缀。** `faa4ad00` 那次「版本对齐上游 4.0.13」起，
> fork 直接采用上游版本线号（当前 4.0.13），后续与上游同基准。下面的 `-trim.N` 规则仅对
> **v3.20.4-trim.1 ~ v3.20.4-trim.13 时代**有效，作为历史记录保留，**不再适用于新发布**。

- **递增**：合并一批上游修复/功能（月度 ADR-001 pick 评审）并重新打包发布时；或 fork 自身发生行为变更的修复/重构后发布时。
- **不递增**：纯文档、注释、CI 配置修正（合入下一批次一起发）。
- 命名（历史规则，已停用）：`v{上游基线版本}-trim.{N}`，如 `v3.20.4-trim.14`。升上游基线（如 4.0）后 N 归零重计：`v4.0.x-trim.1`。
- 命名（现行）：直接用上游线号 `v{上游版本}`，如 `v4.0.13`；fork 自有变更随上游版本一起发布，不另起后缀。

## 7. 跟版流程

1. `git fetch upstream --tags`
2. 若上游 `SCHEMA_VERSION` 变化：先跑 T5 式只读评估，再决定是否跟。**fork 当前 22 与上游 v4.0.6 基线值 20 的偏离（§2 不变量 ②）正是一个待决项**，走跟版时一并处理。
3. 跑 `node scripts/trim-app.mjs <上游tag>` 产出裁剪补丁（T4），按补丁 cherry-pick/合并。
4. 全量闸门：typecheck + cargo check + cargo test --lib + 打包验证。
5. 发布（沿用上游版本线号；`-trim.N` 递增已停用）。
