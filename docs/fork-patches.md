# fork-patches — 本 fork 对上游文件的行内修改登记

> 依据 ADR-001（Accepted，2026-09-29）。凡对上游也存在文件的**行内修改**，
> 在此登记一行：文件 · 位置 · 意图。上游 cherry-pick 发生冲突时，按本清单
> 逐处重建；本清单也是"我们的行为以我们为准"的凭据。
>
> **位置一律登记符号名**（`fn`/`struct`/`impl`/测试名/表列名），**禁止行号**：
> 行号随任何一次无关提交就漂移失效，而符号名可用
> `rg -n "fn <symbol>" src-tauri/src` 在冲突重建时直接定位。
>
> 整文件新增（零冲突）不登记：pi_config/、proxy/session_affinity.rs、
> proxy/circuit_breaker.rs、services/snapshots.rs、deeplink 之外的新命令等。

## 裁剪类（上游功能，本 fork 删除）

- 全仓：仅保留 claude/codex/pi 三 app；gemini/grokbuild/opencode/openclaw/hermes/mcode 已裁剪
  （DB schema 的 CHECK 约束与种子行**有意保留**，服务旧库迁移链）。
- updater 链整体删除（tauri.conf.json / capabilities / Cargo.toml / lib.rs /
  settings.rs / 前端 UpdateContext 等）——发版走 GitHub Releases 手动升级
  （DeepLinkImportDialog/DatabaseUpgrade 指向 langkeshuo-ai releases）。

## 行内修改（按文件）

- `src-tauri/src/database/dao/proxy.rs` · `fn get_proxy_config_for_app` /
  `fn get_proxy_config_for_app_blocking` · 双变体（inline 供 block_on 轮询 /
  `_blocking` 供热路径），db_blocking 约束注释
- `src-tauri/src/database/dao/settings.rs` · get_rectifier_config_on_conn 等
  `_on_conn` 同步核心（H4）
- `src-tauri/src/database/mod.rs` · conn: Arc<Mutex<Connection>> + db_blocking()
  helper（约束：仅 tokio 上下文，禁嵌套）
- `src-tauri/src/proxy/circuit_breaker.rs` · `Transition` 锁序不变量（`config` →
  `state` → `last_opened_at`）+ `fn transition_to_open` +
  `fn test_no_abba_deadlock_between_probe_and_transition_paths` 回归测试
- `src-tauri/src/proxy/provider_router.rs` · Pi defaultProvider 三分支语义 +
  get_or_create 锁外读 DB + L1 host 精确匹配（is_local_proxy_url 任意端口语义）
- `src-tauri/src/proxy/session.rs` · normalize_session_id（128 字符 + 字符白名单）
- `src-tauri/src/proxy/handler_context.rs` · 热路径 DB 读切 blocking + 合并批读
- `src-tauri/src/proxy/forwarder.rs` · 两处 load-bearing clone 注释
- `src-tauri/src/services/proxy.rs` · backup_live_config_strict Pi 分支存原始
  字节（字节级恢复）+ is_local_proxy_url url::Host 重写 + verbatim 家族折叠
- `src-tauri/src/services/provider/mod.rs` · L7 switch() 接管闸门错误传播
- `src-tauri/src/services/snapshots.rs` · capture_provider_id 读 Pi settings.json
  + apply 补写 set_pi_default_provider + sync_mutex 串行化
- `src-tauri/src/pi_config/mod.rs` · set_pi_default_provider /
  parse_models_document_raw / baseUrl 手术式替换（注释保留）
- `src-tauri/src/codex_config.rs` · merge_codex_config_surgical + 多行字面量
  换行保护（L2）
- `src-tauri/src/tray.rs` · refresh_tray_menu spawn_blocking + handle_auto_click
  异步化（L3）
- `src-tauri/src/commands/import_export.rs` · validate_transfer_path（M2）
- `src-tauri/src/deeplink/parser.rs` · provider/prompt app 白名单收紧为
  claude/codex(/prompt 含 pi)（E11）
- `src-tauri/src/commands/snapshots.rs` · emit 已有 log::error（L6，无改动）
- `src/components/proxy/ProxyPanel.tsx` · `const handleSaveBasicConfig` 非回环地址
  保存确认门（H3）
- `src-tauri/tauri.conf.json` · CSP connect-src 收紧到 https://models.dev（M4）
- `tests/msw/handlers.ts` · fixture 对齐 [claude,codex,pi]；get_config_dir 返回
  /default/{app}
- `src/components/settings/SnapshotSection.tsx` · `const refresh` 加载失败 toast（L5）

## 语义分歧登记（cherry-pick 上游功能时其测试的取舍）

- `codex_config_value_is_cc_switch_owned`（a5b3dd3a）· **接受上游安全属性**：
  顶层 `experimental_bearer_token` 切换时无论值一律剥离（新配置缺席时）——
  残留 = A 的凭证发到 B 的端点。推翻了本 fork 引擎原先"仅 PROXY_MANAGED
  占位符可剥离"的自设限制；注释/自定义段保留不变。
- `tests/provider_service.rs::switch_codex_syncs_shared_keys_*`（4540d7e4）·
  **拒绝上游全权重写语义**：上游 autosync 测试断言切换后 live 不得含顶层
  `wire_api` 与 `[mcp_servers]`/`[mcp.servers]`；本 fork 外科手术式合并
  （4f3e38f4）有意逐字保留用户可见配置（`src-tauri/src/codex_config/tests.rs`
  `fn official_proxy_route_uses_native_auth_and_local_responses_provider` 断言
  [mcp_servers] 存活），测试已改为断言 fork 行为。
- 教训：pick 上游 PR 时，其测试断言默认编码上游引擎语义——必须逐条对照
  fork 对应引擎（surgical merge / 保留特性）判定接受或改写，并把本表补记。

## 上游 pick 评审记录（2026-09-29，截至 upstream 846de29c，落后 22）

**已抄（3 笔）：**
- `b9c27393` → b6a1109c：incremental_vacuum 整 freelist 回收（原每次仅回收 1 页）
- `846de29c` → 46cc61dc：skills 图标（Wrench→SkillsIcon；cherry-pick 在裁撤段冲突，手工套用）
- `6064fc1c` → 897f0fcd：删 connectivity check 配置面/批量检查/日志（-444 行；批量与日志在 fork 前端零调用，单次检查改用默认配置）

**拒绝（含理由）：**
- `15c0b3ce` 及其依赖 `b0875f4c`/`08a80b90`/`a79d9ff1`：direct/proxy mode 架构（mode/*、*_direct.rs 在 fork 不存在）——ADR-001 底线：保 backup-restore 中间人能力
- `2185498e`：上游泄漏修复针对其 key-field writer；fork 已由 a5b3dd3a 在 surgical 引擎侧覆盖同一安全属性
- `81df5a08`/`0e6430ab`/`63ed5002`/`fb564537`/`143bc461` + 3 docs：key-field switching 引擎与 fork surgical 引擎对立
- `69ff69dd`：40 文件重构绑上游 editor 架构，fork 侧 useDraftEditorProjection 等锚点不存在

**候选待评审（月度）：**
- `46fa2e0e` 删成本倍率（-866）：功能移除非修复；与 H4 DAO/schema 列/计价链路交织，7 文件冲突，需连带决定 proxy_config.default_cost_multiplier 列去留——单独会话做
- `46fa2e0e`/`6064fc1c` 同类上游删除面继续积累时重估
