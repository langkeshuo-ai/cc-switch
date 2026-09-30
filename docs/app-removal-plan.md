# 7 应用移除 — 真实剩余清单与迁移设计

> 状态：**侦察完成，待执行**。基线 `cd160f5c`（= `origin/main`）。
> 本文替换 brief 中的"已扫描落点"清单——那份清单基于**上游/旧快照**，与当前 fork 差异很大。

## 0. 结论先行

| 层 | brief 假设 | 实际 |
|---|---|---|
| 第 1 层 运行时数据 | 待做 | **完全未做** |
| 第 2 层 DB schema | 待做 | **完全未做**（此前 trim 有意保留，见 §7.1） |
| 第 3 层 Rust 后端 | 主体待做 | **主体已完成**（`AppType` 已三值）；剩 `commands/misc.rs` 等收尾 |
| 第 4 层 前端 | 主体待做 | **主体已完成**（`AppId`/`VisibleApps`/tab 已三值）；剩 AboutSection + i18n + usage 类型 |
| 第 5 层 测试与文档 | 待做 | 少量陈旧测试 + 大量 docs |

**最大工作量来源**：`src-tauri/src/commands/misc.rs`（7977 行，工具生命周期支持 9 个工具）。

---

## 1. 勘误表（brief 落点 → 实际）

| brief 落点 | 实际状态 | 证据 |
|---|---|---|
| `AppType` 枚举含 6 个待删变体 | 已是 `Claude` / `Codex` / `Pi` | `app_config.rs` `enum AppType` |
| `AppType::from_str` 白名单 | 已收紧为三值 + localized 错误 | `app_config.rs` `impl FromStr for AppType` |
| `src-tauri/src/opencode_config.rs` | **不存在** | `find src-tauri/src -name opencode_config.rs` → 空 |
| `src-tauri/src/mcp/opencode.rs` | **不存在**（仅 `claude.rs`/`codex.rs`/`mod.rs`/`validation.rs`） | `ls src-tauri/src/mcp/` |
| `src-tauri/src/session_manager/providers/opencode.rs` | **不存在**（仅 claude/codex/pi/utils/mod） | `ls` |
| `src-tauri/src/services/session_usage_opencode.rs` | **不存在**（仅 `session_usage.rs`/`_codex.rs`/`_pi.rs`） | `ls` |
| `src/config/opencodeProviderPresets.ts` + 测试 | **不存在** | glob 无命中 |
| `VisibleApps` 旧 json 反序列化风险 | 已 3 字段 + `#[serde(default = "default_true")]`；serde 默认忽略未知键 | `settings.rs` `struct VisibleApps`；`src/types.ts` `interface VisibleApps` |
| `CommonConfigSnippets` 含 opencode | 已只剩 `claude`/`codex` | `app_config.rs` `struct CommonConfigSnippets` |
| 官方占位种子含 gemini 等 | 已只剩 `claude-official`/`codex-official` | `dao/providers_seed.rs` `OFFICIAL_SEEDS` |
| deeplink `build_*_settings` 分支 | 已移除（grep 无 `build_gemini_settings` 等） | — |
| 前端 tab / presets / 路由 | 已移除；`AppId` 已三值 | `src/lib/api/types.ts` `type AppId`；`src/config/appConfig.tsx` `APP_IDS` |
| `providers` 表 CHECK 含 gemini/grokbuild | **该表无 CHECK 约束**（`app_type TEXT NOT NULL`） | `schema.rs` `CREATE TABLE providers` |
| `settings` 键 `common_config_opencode` 等 | 代码中**零引用**，仅存于旧库 | grep 无命中 |

---

## 2. 第 1 层 — 运行时数据清理（未做）

需要删的行：

| 表 | 条件 | 备注 |
|---|---|---|
| `providers` | `app_type IN ('claude-desktop','gemini','grokbuild','opencode','openclaw','hermes','mcode')` | 无 CHECK，直接 DELETE |
| `provider_endpoints` | 同 app_type | FK `ON DELETE CASCADE` → `providers`；`PRAGMA foreign_keys=ON` 已开（`database/mod.rs`），仍建议显式 DELETE 保险 |
| `provider_health` | 同 app_type | 同上 |
| `proxy_config` | `app_type IN ('gemini','grokbuild')` | 有 CHECK，需配合 §3 重建 |
| `settings` | `common_config_opencode`、`gemini_common_config_credentials_scrubbed_v1` | 代码零引用 |

**保留** `official_providers_seeded`：它是"只播种一次"的幂等 flag（`dao/providers.rs` `get_bool_flag`）。删掉会触发重播，但重播源 `OFFICIAL_SEEDS` 已只剩 claude/codex，重播无害——**无收益的破坏，故保留**。

### ⚠️ 复活源（必须同步处理，否则数据会重新长出来）

1. `schema.rs` `create_tables_on_conn`：`INSERT OR IGNORE INTO proxy_config ... VALUES ('gemini', ...)` 与 `VALUES ('grokbuild', ...)` 两条 seed
2. `dao/proxy.rs` `seed_default_proxy_config_rows`：同款两条 seed（与上条重复定义，两处都要改）

---

## 3. 第 2 层 — 数据库 schema（未做）

`SCHEMA_VERSION`：`21` → `22`，新增 `fn migrate_v21_to_v22`（`schema.rs`）。

| 对象 | 改动 | 方式 |
|---|---|---|
| `mcp_servers` | 删 `enabled_gemini` / `enabled_grokbuild` / `enabled_opencode` / `enabled_mcode` / `enabled_hermes` | 重建表（SQLite 无 DROP COLUMN 前向兼容保证） |
| `skills` | 同上 5 列 | 重建表 |
| `proxy_config` | CHECK 收紧为 `IN ('claude','codex','pi')` | 重建表 |
| `providers` / `provider_endpoints` / `provider_health` | 无 schema 改动 | 仅 §2 数据删除 |

**全新库**：同步改 `create_tables_on_conn` 的 3 处 `CREATE TABLE`。

**索引/触发器**：`mcp_servers`、`skills`、`proxy_config` 三表**均无**索引或触发器（已 grep 确认），重建时无需搬运。

### ⚠️ 历史迁移链 v0..v21 一行不能删

`apply_schema_migrations` 用 `while version < SCHEMA_VERSION` 逐步查表；**缺任一步**即报
`"未知的数据库版本 {version}"`。因此：

- `migrate_v3_to_v4`(opencode) / `migrate_v9_to_v10`(hermes) / `migrate_v13_to_v14`(grokbuild) /
  `migrate_v14_to_v15`(grokbuild) 必须原样保留，其中重建 `proxy_config` 的旧 CHECK
  `IN ('claude','codex','gemini','grokbuild')` 也必须保留——否则历史库升不上来。
- 连带结论：**验收标准 #7「源码零标识符」与迁移链互斥**，见 §7.2。

### ✅ 顺序安全性（已核对，无坑）

`create_tables_on_conn` 刻意不 seed `pi` 行，注释写明理由：`v13_to_v14` 重建含旧 CHECK 的表并整行搬数据，提前存在的 `pi` 行会撞约束。`pi` 行由 `v20_to_v21` 补齐。
→ 新增 `v21_to_v22` 排在最后，此时 `pi` 行已存在，重建 CHECK 为 `('claude','codex','pi')` 可正常保留它。

### 连带必须改的 SQL / struct

| 位置 | 改动 |
|---|---|
| `dao/mcp.rs` `MCP_SERVER_SELECT` | 14 列 → 9 列（去掉 5 个 `enabled_*`） |
| `dao/mcp.rs` `row_to_mcp_server` | 删除 `row.get(9)..row.get(12)` 四行（`_enabled_gemini` 等） |
| `dao/mcp.rs` `save_mcp_server` | INSERT 的 9 参数**不变**（本就只写 claude/codex）——brief 担心的"参数错位"实际风险在 SELECT 侧 |
| `dao/skills.rs` `get_all_installed_skills` | SELECT 18 列 → 13 列；`row.get(14/15/16)` 重排为 `9/10/11` |
| `dao/proxy.rs` `seed_default_proxy_config_rows` | 删 gemini / grokbuild 两条 INSERT |
| `dao/proxy.rs` 默认值 match | 删 `"grokbuild" => (3, 60, ...)` 分支 |
| `database/tests.rs` | 手工建表语句含 `enabled_opencode` → 同步 |

---

## 4. 第 3 层 — Rust 后端残留

### 4.1 需要移除

| 位置 | 内容 |
|---|---|
| `commands/misc.rs` | `VALID_TOOLS: [&str; 9]` → 4 项（`claude/codex/pi` + ?，需定）；`npm_install_command_for`、`npm_package_for`、`npm_install_extra_args`、`official_update_args`、`tool_display_name`、`build_tool_lifecycle_command` 的 gemini/grok/opencode/openclaw/mcode 分支；常量 `OPENCODE_INSTALL_UNIX`/`GROK_INSTALL_UNIX`/`MCODE_INSTALL_UNIX`/`HERMES_INSTALL_UNIX`/`HERMES_UPDATE_UNIX`/`HERMES_INSTALL_WINDOWS_SCRIPT`/`GROK_INSTALL_WINDOWS_SCRIPT`/`MCODE_INSTALL_WINDOWS_SCRIPT` + 对应 `fn *_install_windows_command` / `mcode_installer_update_command` / `hermes_update_windows_command`；`brew_formula_from_path` 的 gemini 特例；**以及 misc.rs 内数十条相关断言测试** |
| `deeplink/parser.rs` | MCP `apps` 白名单仍把 `gemini/grokbuild/grok/opencode/openclaw/hermes` 列为**合法值**，错误串同样列举 → 收紧为 `claude/codex`，错误串同步 |
| `codex_config.rs` | 注释引用已不存在的 `mcp/grokbuild.rs`、`opencode_config.rs` → 改写 |

### 4.2 保留（承重 / 防回归资产）

| 位置 | 理由 |
|---|---|
| `app_config.rs` 单测（断言 `"mcode"`/`"claude-desktop"`/`"hermes"` 解析失败 + 反序列化降级为 `claude`） | 防回归资产，正是"移除后仍要优雅拒绝"的证据 |
| `deeplink/mcp.rs` 已移除应用静默忽略分支 | 语义正确（旧 deeplink 不炸），仅核对文案 |
| `proxy/providers/mod.rs` `ProviderType::GeminiCli` | **代理的 Gemini 协议适配器**，服务 claude/codex/pi 的上游转换 |
| `proxy/handlers.rs` `api_format == "gemini_native"` + `transform_gemini` / `streaming_gemini` / `gemini_shadow` | 同上，协议转换承重 |
| `proxy/session.rs` `grokbuild` client_format / session 前缀 | 会话 ID 提取的协议分支 |
| `usage_stats.rs` `_gemini_session` / `_opencode_session` / `_mcode_session` 标签与相关 SQL | 历史用量行的展示标签；删除会让**旧数据渲染退化** |

---

## 5. 第 4 层 — 前端残留

| 位置 | 改动 |
|---|---|
| `components/settings/AboutSection.tsx` | `POSIX_ONE_CLICK_INSTALL_COMMANDS` / `WINDOWS_ONE_CLICK_INSTALL_COMMANDS` 含 9 个 CLI → 裁到 claude/codex/pi；`MCODE_WINDOWS_INSTALL_COMMAND` / `MCODE_NPM_INSTALL_COMMAND` / `HERMES_*` 常量随之删 |
| `i18n/locales/{zh,zh-TW,ja,en}.json` | ① `settings.oneClickInstallHint` 列举 9 应用 → 裁；② `sessions.subtitle` 列举 → 裁；③ `usage.appFilter` 含 `gemini`/`opencode`/`mcode` → 裁为 claude/codex/pi；④ `profiles.switcherTooltip` + `profiles.createDescription` 含 `claude-desktop` 键 → 删 |
| `types/usage.ts` | `UsageApp` 联合类型与两个数组含 7 应用 → 裁；**需先确认后端是否仍会发出这些 key** |
| `types.ts` | `opencodeConfigDir` / `openclawConfigDir` / `hermesConfigDir` → 删；消费点 `hooks/useSettings.ts`、`hooks/useSettingsForm.ts` 同步 |
| `components/BrandIcons.tsx` | `OpenClawIcon` + `claw.svg` 引用 → 删（确认无其他消费点） |
| `App.tsx` | 注释 `for additive mode apps like OpenCode/OpenClaw` → 改写为 Pi |

### ⚠️ 绝对保留（删了会坏）

| 位置 | 理由 |
|---|---|
| `i18n` 的 `opencode` **命名空间**（`npmPackage`/`baseUrl`/`headers`/`models`…） | **`PiProviderForm.tsx` 与 `RequestHeadersEditor.tsx` 正在用** `t("opencode.baseUrl")` / `t("opencode.headers")` —— 命名历史遗留，内容是 Pi 供应商表单文案。删了 Pi 表单直接坏 |
| `claudeProviderPresets.ts` / `codexProviderPresets.ts` 的 `"OpenCode Go"` 预设（`opencode.ai/go`） | **上游服务商**（一个订阅服务），与 `opencode` AppType 无关 |
| `codexProviderPresets.ts` 中"官方 OpenCode/OpenClaw 接入页"注释 | 说的是上游服务商文档页 |

---

## 6. 第 5 层 — 测试与文档

| 位置 | 改动 |
|---|---|
| `deeplink/tests.rs` `test_parse_grokbuild_mcp_deeplink` | **已失效**：断言 `apps == "grokbuild"` 且解析成功 → 改为断言被拒绝 |
| `deeplink/tests.rs` `parse_mcp_apps("claude,gemini,mcode")` 静默忽略用例 | 白名单收紧后语义由"忽略"变"报错" → 重写 |
| `deeplink/tests.rs` prompt `app=grokbuild` 报错用例 | 保留，核对文案 |
| `schema.rs` 迁移测试（v13→v14 / v14→v15 / v20→v21） | **保留**（测的是历史迁移）；新增 v21→v22 测试 |
| `database/tests.rs` 手工建表 | 同步列裁剪 |
| 前端 `tests/components/{UnifiedMcpPanel,UnifiedSkillsPanel}.test.tsx`、`tests/hooks/useSettings*.test.tsx` 等 | 清理 gemini/opencode 夹具 |
| `docs/user-manual/{zh,en,ja}/**`、`README*.md`、`README_{ZH,JA,DE}.md` | 文档层批量裁剪；`CHANGELOG.md` 与 `docs/release-notes/*` **豁免**（历史记录不可改） |

---

## 7. 硬约束冲突与风险清单

### 7.1 schema 层此前是**有意保留**的

`docs/fork-patches.md`「裁剪类」明确写着：

> （DB schema 的 CHECK 约束与种子行**有意保留**，服务旧库迁移链）。

本次任务要求把它们也移除 → 属于**推翻既有决策**。技术上可行（见 §3），但代价是三表重建 + 旧库迁移回归。

### 7.2 验收 #7「源码零标识符」**不可达成**

三个硬性例外：

1. **历史迁移链**（§3）——保留字样是旧库能升级的前提；
2. **代理的 Gemini 协议适配**（§4.2）——`ProviderType::GeminiCli` / `gemini_native` / gr okbuild session 分支服务 claude/codex/pi 的上游转换，删了破坏硬约束 #2；
3. **Pi 表单复用的 `opencode` i18n 命名空间**（§5）——删了破坏硬约束 #2。

外加两个软例外：上游服务商预设 "OpenCode Go"、历史用量标签。

→ 建议把上述清单登记为 #7 的**显式豁免名单**，并把判定脚本写成"豁免清单外零命中"。

### 7.3 版本串不一致

`package.json` = `3.20.4-trim.3`，`Cargo.toml` = `3.20.4-trim.3`，`tauri.conf.json` = `3.20.4-trim.5`。
brief 提到"启动时会按二进制版本重置 settings.json"——**当前源码 grep 不到该逻辑**（updater 链已整体删除，见 fork-patches.md）。需实测确认，可能已不适用。

### 7.4 接管残留恢复

`proxy_config` 删 `gemini`/`grokbuild` 行后，需确认 `services/proxy.rs` 的 backup/restore 与"上次异常退出"恢复逻辑不依赖这两行存在。预期安全（该逻辑只对 claude/codex/pi 工作），但必须跑测试确认。

### 7.5 真实数据

本机 `~/.cc-switch/cc-switch.db` = **61.8 MB**，含真实供应商数据。
迁移前必须先备份（代码已有 `database/backup.rs` 的 pre-migration backup 机制，会输出 `Creating pre-migration database backup (v21 → v22)`）。

---

## 8. 建议执行顺序

1. （前置）统一版本串到 `3.20.4-trim.6`；备份 DB。
2. **第 1+2 层一起做**：同一个 `v21_to_v22` 迁移里删数据 + 重建三表；同步改 DAO 与 CREATE TABLE。
3. 跑 `cargo test --lib database::` + `--lib services::proxy` + `--lib services::provider` 定向验证；用真实 DB 副本实测旧库升级无 panic。
4. **第 3 层**：`commands/misc.rs` 裁剪 + deeplink 白名单收紧 + 注释修正。
5. **第 4 层**：AboutSection + i18n + usage 类型 + BrandIcons + types.ts。
6. **第 5 层**：陈旧测试改写 + docs 批量裁。
7. 五道闸门 + `cargo build` + 前端 `build`；功能回归（验收 #5）。
