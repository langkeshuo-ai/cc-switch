# Pi 代理层删除清单（2026-10-10，T3 产出）

> 任务来源：重构维护模型（见 `BASELINE.md`）。本清单逐项记录删除位置与原因。
> 硬性约束：`❌ 不得把 'pi' 加回 proxy_config 的 CHECK 约束或 PROXY_APPS`。

## 1. 整文件删除

| 文件 | 内容 | 原因 |
|---|---|---|
| `src-tauri/src/proxy/providers/pi.rs`（186 行） | `PiAdapter`，`/pi/anthropic`、`/pi/openai` 透传适配 | fork 自造的 Pi 网关适配器；上游 PR #6064 明确 Pi 无 gateway/routing |

## 2. `src-tauri/src/proxy/`（fork 自造 Pi 代理面）

| 文件 | 删除内容 | 原因 |
|---|---|---|
| `server.rs` | `/pi/anthropic/*rest`、`/pi/openai/*rest`、`/pi/openai/chat/completions`、`/pi/openai/responses` 四条路由及注释 | 网关路径常量（约束禁止项） |
| `handlers.rs` | `handle_pi_messages` / `handle_pi_chat_completions` / `handle_pi_responses` + 5 个 `pi_openai_*` 测试 | 网关入口 |
| `provider_router.rs` | `pi_proxy_override` 解析块（settings.json defaultProvider 优先路由）+ 4 个 `pi_*` 测试 + `write_pi_settings` 辅助 | Pi 专属路由逻辑 |
| `handler_context.rs` | `current_provider_id` 的 Pi 分支 | 同上 |
| `providers/mod.rs` | `mod pi;`、`get_adapter` 的 `PiAdapter` 臂 | 适配器注册 |

保留的防御性 Pi 臂（非代理路径，match 穷尽性要求，语义为"Pi 不参与"）：
`providers/mod.rs:194`（failover 支持判定，返回 None）、`providers/mod.rs:248`（get_adapter，返回 None）。

## 3. `src-tauri/src/services/proxy.rs`

删除 6 个 fork 自造 Pi 函数：`resolve_pi_takeover_target`、`reconcile_pi_sources`、
`read_pi_models_document`、`pi_models_document_is_taken_over`、
`restore_pi_base_url_from_ssot`、`warn_pi_restore_without_archive`。

删除/归一 17 处 Pi match 臂与分支：

- `get_takeover_status`：删除 `pi_enabled` 读取与 `ProxyTakeoverStatus.pi` 字段。
- `backup_live_config_strict` / `takeover_live_config_strict` /
  `takeover_live_config_best_effort` / `restore_live_config_for_app_inner` /
  `restore_live_config_for_app_with_fallback`：Pi 臂改为防御性 no-op（Pi 无本地代理，不应到达）。
- `restore_live_configs`：应用列表 `[Claude, Codex, Pi]` → `[Claude, Codex]`。
- `detect_takeover_in_live_config_for_app` / `live_has_proxy_placeholder_for_app` /
  `cleanup_takeover_placeholders_in_live_for_app` / live-taken-over 匹配：
  Pi 臂改为 `false` / `Ok(false)` / `Ok(())`（models.json 不会被本网关改写，无占位符概念）。
- `sync_live_to_provider`：Pi 早退保留，注释改为防御性说明。

连带删除：`services/provider/mod.rs` 的 `validate_pi_provider_for_reconcile`
（唯一调用方是已删的 `reconcile_pi_sources`）。

测试：`tests.rs` 删除 Pi 接管测试块 397 行（6 个辅助 + 11 个 `pi_*` 测试）。

## 4. `src-tauri/src/pi_config/mod.rs`

删除 fork 自造 takeover 辅助：`parse_models_document_raw`、`read_models_document_raw`、
`restore_models_document_raw`、`pi_takeover_prefix_for_api`、`apply_pi_takeover_base_url`、
`set_pi_provider_base_url`、`write_provider_base_url_surgical` 及 9 个 takeover 测试。

保留（非代理语义）：

- `pi_proxy_current_provider_key` → **改名 `pi_native_current_provider`**（26 处调用点同步）。
  函数体不变：读 settings.json `defaultProvider`。改名原因：原名中的 "proxy" 是说谎命名，
  该函数与代理转发无关，是 Pi 当前供应商的权威读取口（tray/profile/snapshot/provider 服务共用）。
- `set_pi_default_provider`：快照 apply 时让 Pi 真正切换的写点（`services/snapshots.rs`）。
- `replace_unique_string_literal` / `insert_field_after_first_brace`：上一项的手术式写辅助。

## 5. 其他后端

| 文件 | 改动 | 原因 |
|---|---|---|
| `commands/failover.rs` | 删除 Pi 早退守卫（17-21 行） | `supports_local_proxy` 回退两应用后已天然拒绝 Pi |
| `app_config.rs` | `supports_local_proxy`：`Claude \| Codex \| Pi` → `Claude \| Codex`（回退上游） | Pi 无代理数据面 |
| `proxy/types.rs` + `src/types/proxy.ts` | `ProxyTakeoverStatus` 删 `pi` 字段 | 状态源只剩 claude/codex |
| `src/config/appConfig.tsx` | `ProxyAppId`/`PROXY_APP_IDS` 收窄为 `["claude","codex"]` | **护栏项**：pi 不得出现在 PROXY_APPS 等价物 |

## 6. 文档

| 文件 | 改动 |
|---|---|
| `docs/ssot.md` | §2 能力矩阵 Pi 本地路由 ✅→❌；§3.5 重写为"已删除"存档；§4 路由表删 `/pi/*`；§5/§6 更新；新增 Pi 护栏自检项（含"Pi 没有路由不是 bug"警示） |
| `docs/PI-ROUTING.md` | 新建：Pi 实际路由方式（models.json 手写 baseUrl 接入官方网关） |
| `README_ZH.md` / `README.md` / `README_JA.md` | Pi 行功能表核对通过（本地路由/托盘/MCP = –、Skills = ✓ 与代码一致），无需改动 |

## 7. 未删除的 "pi" 引用（核对为合理保留）

- `app_config.rs` 等处的 `AppType::Pi` / `is_additive_mode`：Pi 作为配置管理对象的
  基础支持（上游 #6064 设计），保留。
- `commands/misc/tool_version.rs`：Pi CLI 版本检测（npm registry 查询），保留。
- `settings.rs` / `tray.rs` / `commands/*` / `services/provider/*`：经
  `pi_native_current_provider` 读 Pi 生效供应商，保留（已改名）。
- `session_manager/providers/pi.rs`、`services/skill.rs` Pi 分支：会话/_skills 功能，保留。
- `commands/snapshots.rs`、`services/snapshots.rs`：快照对 Pi 的 capture/apply，保留。
