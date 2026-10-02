# SSOT：本 fork 的能力边界（Single Source of Truth）

> 建立日期：2026-10-02
> 目的：把"本 fork 到底支持哪些 app、每个 app 的能力差异、权威源在哪"收敛到一处，
> 供改代码 / 改文档 / 排障时**先查这里再动手**，避免第二次写出与实现不符的文档。

**取证方式**：下表所有结论均来自对当前 `main` 的 `grep` 实测，不是从上游文档抄的。
复核命令附在每节末尾。

---

## 1. App 集合：3 个（claude / codex / pi）

| 层 | 权威定义 | 实测值 |
|----|---------|-------|
| 后端枚举 | `src-tauri/src/app_config.rs` `enum AppType` | `Claude` / `Codex` / `Pi` |
| 前端常量 | `src/config/appConfig.tsx` `APP_IDS` | `["claude","codex","pi"]` |
| 工具版本管理 | `src-tauri/src/commands/misc/tool_version.rs` `VALID_TOOLS` | `["claude","codex","pi"]` |

```bash
grep -n "enum AppType" -A6 src-tauri/src/app_config.rs
grep -n "VALID_TOOLS" src-tauri/src/commands/misc/tool_version.rs
grep -n "APP_IDS" src/config/appConfig.tsx
```

**已裁剪的 app**（本 fork 不支持，文档/代码出现即为错）：
`gemini` / `grokbuild` / `grok` / `opencode` / `openclaw` / `hermes` / `mcode`（MiniMax Code）
/ `claude-desktop`（Claude Desktop）

> ⚠️ **`gemini` 仍出现在两处且是合理的**，不要误删：
> 1. `proxy_native` 协议格式（`gemini_native`）——本地路由确实做 Gemini 协议转换，
>    代码在 `proxy/providers/transform_gemini.rs` + `streaming_gemini.rs`，被
>    `handlers.rs` / `claude.rs` 实际调用。
> 2. 模型定价表里的 Gemini 3 / 2.5 系列。
>
> 判据：**作为"被管理 app"已裁剪；作为"协议格式"与"模型名"仍在用。**

---

## 2. 能力矩阵（差异点必须按此写文档/做 UI）

| 能力 | Claude | Codex | Pi | 权威来源 |
|------|:---:|:---:|:---:|---------|
| Provider 模式 | 切换 | 切换 | **累加** | `AppType::is_additive_mode` |
| 本地路由 | ✅ | ✅ | ✅ | `PROXY_APP_IDS` |
| **故障转移** | ✅ | ✅ | ❌ | `FAILOVER_APP_IDS`（前端 `appConfig.tsx`） |
| MCP | ✅ | ✅ | ❌ | `MCP_APP_IDS = ["claude","codex"]` |
| Skills / Prompts / Sessions | ✅ | ✅ | ✅ | `SKILLS_APP_IDS` |
| Pi provider deeplink 导入 | ✅ | ✅ | ❌ | `deeplink/parser.rs:85` 只收 `claude`/`codex` |

**故障转移的 UI 门控**：必须用 `isFailoverAppId(appId)`，不要用 `isProxyAppId`。
Pi 走透明转发，不参与故障转移——早期版本用 `isProxyAppId` 导致"能点必失败"。
（`src/components/providers/ProviderList.tsx` / `settings/ProxyTabContent.tsx`）

---

## 3. 「当前供应商」权威源（最容易踩坑的一张表）

各 app 的当前生效供应商**存在不同地方**，混用会静默返回 `None` 或错误值。

| app | 权威源 | 读取函数 | 说明 |
|-----|-------|---------|------|
| Claude | 设备级 settings 字段 | `settings::get_current_provider` | 有 DB `is_current` 兜底 |
| Codex | 设备级 settings 字段 | 同上 | 同上 |
| **Pi** | **`~/.pi/agent/settings.json` 的 `defaultProvider`** | `pi_config::pi_proxy_current_provider_key` | 设备级字段恒为 `None`；DB `is_current` **不是** Pi CLI 跟随的源 |

**唯一正确的取法**（2026-10-02 起）：

```rust
// 遍历 app 的通用路径（tray / profile / snapshot / failover / provider 增删 …）
settings::get_effective_current_provider_with(
    db,
    app_type,
    pi_config::pi_proxy_current_provider_key,   // Pi 分流在此内聚
)
```

- `get_effective_current_provider_with` 内部用 `current_provider_lives_in_settings`
  判定是否问注入的权威源（Pi 为 `false`），读不到时 warn 并退回 DB `is_current`
  （与 `proxy::provider_router` 的既有兜底一致）。
- 之所以做成"调用方注入"而不让 `settings` 直接调 `pi_config`：`pi_config` 已经
  依赖 `settings`（取 override 目录），反向调用会形成模块循环依赖。
- 旧的 `get_effective_current_provider`（无注入）**只允许**在硬编码
  `AppType::Claude` / `AppType::Codex` 的路径里出现。

```bash
grep -rn "get_effective_current_provider(" src-tauri/src/ | grep -v "_with"   # 应只剩 Claude/Codex 硬编码 + 定义 + 测试
```

---

## 3.5 Pi 三源权威表（2026-10-03 新增，接管自愈收敛）

Pi 的"同一个供应商"在三处各有一份数据。**三源允许漂移**，接管流程负责收敛；
**禁止要求用户手工预对齐**。

| 源 | 角色 | 权威范围 | 读取函数 |
|----|------|---------|---------|
| `~/.pi/agent/settings.json` 的 `defaultProvider` | **Pi CLI 唯一跟随的生效源** | 谁在实际生效 | `pi_config::read_pi_native_defaults` / `pi_proxy_current_provider_key` |
| `~/.pi/agent/models.json` 的 `providers[key]` | **供应商真身**（自带 `api` 方言、`apiKey`） | 供应商定义 | `pi_config::read_pi_native_provider` |
| DB `providers(app_type='pi')` | **管理面镜像** | UI 展示 / 用量统计 / 失败转移 / 还原源 | `db.get_provider_by_id(key, "pi")` |

### 收敛规则（`ProxyService::reconcile_pi_sources`）

**唯一写点**：`enable_takeover` → `takeover_live_config_strict` 的 Pi 分支，
且仅在 `resolve_pi_takeover_target` 成功之后。`disable_takeover` / 状态查询 /
`get_effective_current_provider*` 等**读路径一律不得调用**（否则轮询状态就写库）。

| 原生节点 | DB 档案 | 行为 |
|:---:|:---:|------|
| 有 | 无 | 从 `models.json` 节点补建档案（节点是真身，DB 是镜像） |
| 无 | 有 | 按档案回填 `models.json` 节点（**唯一允许的 DB→原生方向**） |
| 有 | 有 | **零改动**；仅 `api` 漂移时 `log::warn!` |
| 无 | 无 | 不可能——`resolve` 已拦截并给出四段诊断 |

补建档案前**必须**走 `ProviderService::validate_pi_provider_for_reconcile`
（转调既有 `validate_provider_settings(&AppType::Pi, ·)`），不复制校验逻辑。

**锁序**：`set_takeover_for_app`（`services/proxy.rs:1114`）先取 switch 锁，
`save_provider`（`database/dao/providers.rs:181`）内部才取 DB 锁
→ 顺序恒为「switch 锁 → DB 锁」。**严禁**在持有 DB 连接锁时调用收敛。

### `api` 方言的四态决策（`resolve_pi_takeover_target`，纯读无副作用）

| 原生 `api` | 档案 `api` | 采用 | 理由 |
|:---:|:---:|------|------|
| 有 | 有且一致 | 两者 | 无歧义 |
| 有 | 有但不一致 | **DB 档案** | 与接管前既有行为一致；`warn!` 打出两侧原文 |
| 有 | 无 | **原生** | 关键：不再因"没有档案"而失败 |
| 无 | 有 | 档案 | 收敛会同时回填原生节点 |
| 无 | 无 | `Err` | 四段诊断（settings.json / models.json / CC Switch / 可用修复） |

### 三条"看着像 bug 其实不是"

| 现象 | 真相 | 依据 |
|------|------|------|
| 接管后 `models.json` 注释全没了 | **只在无法做手术式替换时**才整篇重写（JSON5 定位不到替换点）。备份槽保存的是**原始字节**，停止接管经 `restore_models_document_raw` 原样写回，注释与格式不丢失 | `backup_live_config_strict` 的 Pi 分支 |
| 原生 `api` 与档案 `api` 不一致 | **不是 bug**，以 DB 档案为准并 warn。`models.json` 是真身但接管路由读的是档案；强行"修正"任一侧都是改写用户数据 | 本节四态决策表 |
| 接管只改了 `baseUrl` 一个字段 | **有意为之**。`apiKey` 由 Pi 原生持有、`api` 决定方言、`models` 列表是用户资产——三者都不该被代理接管改写。放宽此约束会连带改写凭据 | `pi_config::apply_pi_takeover_base_url` 及其单测 |

### 档案读取点审计（2026-10-03，`get_provider_by_id(…, "pi")`）

```bash
grep -rn 'get_provider_by_id(.*"pi"\|get_provider_by_id(&provider_key' src-tauri/src
```

生产代码 3 处，**已全部消除"静默放弃"**：

| 位置 | 分类 | 处置 |
|------|------|------|
| `services/proxy.rs` `resolve_pi_takeover_target` | 容错 | 档案缺失不再致命；读失败降级为 `warn` + 三源决策 |
| `services/proxy.rs` `reconcile_pi_sources` | 收敛写点 | 仅显式开接管时调用 |
| `services/proxy.rs` `restore_pi_base_url_from_ssot` | **原为静默** | **已修**：档案缺失时基于原生节点判断残留并 `warn!` 给出修复指引，不再静默 `Ok(false)` |
| `proxy/provider_router.rs:76` | 容错 | 档案缺失 → `warn` → 退回 DB current（运行期路由既有行为，正确） |
| `services/provider/pi.rs` 7 处 | 测试 | 均为 `mod tests` 内的 `"cc-switch-test"` 断言，非生产路径 |

**为什么"静默"是缺陷**：停止接管时若静默 `Ok(false)`，用户看到"关闭成功"，
但 Pi 的 `baseUrl` 仍指向本地网关——请求全部打到已停止的网关，表现为"Pi 突然
不能用"，且没有任何线索指向真实原因。

---

## 4. 本地路由端点

监听 `127.0.0.1:15721`（可配）。已注册的路由（`proxy/server.rs`）：

| 路径 | 用途 |
|------|------|
| `/v1/messages`、`/claude/v1/messages` | Anthropic Messages |
| `/pi/anthropic/*rest` | Pi → Anthropic（透传 `/v1/messages` 等原始路径） |
| `/pi/openai/*rest` | Pi → OpenAI（**通配透传**，保留客户端 `/v1`；见下） |
| `/pi/openai/chat/completions`、`/pi/openai/responses` | Pi → OpenAI 兼容入口（无 `/v1` 的老客户端） |
| `/chat/completions`、`/v1/chat/completions` | OpenAI Chat |
| `/models`、`/v1/models` | 模型列表 |
| `/health`、`/status` | 运维 |

> ⚠️ **Pi openai 方言必须透传 `/v1`**（2026-10-03 修正）。`PiAdapter::build_url` 是
> 纯拼接 `{base}/{endpoint}`，若 handler 把 endpoint 硬编码成 `/chat/completions`，
> 上游会收到 `{base}/chat/completions`——对 New API 一类兼容网关这会命中**网页路由**
> 返回 HTML（实测 200 + `text/html`，看起来"成功"但内容是首页）。
> `{base}/v1/chat/completions` 才返回 JSON。
> 因此 openai 两条路由改为 `*rest` 通配 + `strip_prefix`（与 anthropic 同构），
> handler 内 `endpoint_for_upstream` 只剥网关前缀、保留客户端原始路径。
> Codex/Claude 路径传 `None`，行为不变（`endpoint_with_query` 仍是 canonical + 原 query）。

```bash
grep -n "\.route(" src-tauri/src/proxy/server.rs
```

**已裁剪 app 的端点不存在**（文档里若出现 `…/grokbuild/v1`、`GOOGLE_GEMINI_BASE_URL`
指向本地端口，即为过时内容）。

```bash
grep -n "\.route(" src-tauri/src/proxy/server.rs
```

---

## 5. 已知"看起来像 bug 其实不是"的点

| 现象 | 真相 | 依据 |
|------|------|------|
| `gemini` 出现在代码/文档里 | 协议格式与模型定价，非 app | `transform_gemini.rs` 被实际调用 |
| `#[allow(dead_code)]` 有 80+ 处 | 绝大多数是 thiserror 枚举变体 / 平台条件 / 错误码字典 / 测试支撑 | 分类统计见下 |
| `models/anthropic.rs` 曾整体存在 | **已删除**（227 行，零引用，裁剪遗留） | 2026-10-02 |
| `get_provider_config_path` 曾存在 | **已删除**（零引用含测试） | 2026-10-02 |
| `OAuthCredentials` 曾有 4 字段 | **已删到只剩 `access_token`**（本 fork 不做 refresh 交换） | `claude.rs` GoogleOAuth 分支注释 |
| Pi 接管报"在 CC Switch 中没有档案，无法接管" | **已修**（v3.20.4-trim.11 前）。档案只是管理面镜像，缺它不代表 Pi 侧没有该供应商；现在由 `models.json` 节点兜底并在开接管时补建档案 | §3.5 |

### `#[allow(dead_code)]` 分类（2026-10-02 实测 86 处 → 已删 3 类）

| 类别 | 处数 | 处置 |
|------|---:|------|
| 平台条件 `cfg_attr(windows/not(windows))` | 22 | 保留（跨平台编译必需） |
| thiserror 枚举变体（`proxy/error.rs`） | 7 | 保留（错误类型契约，删会破坏匹配完备性） |
| 错误码字典（`proxy/log_codes.rs`，27 码 11 个未引用） | 1（模块级） | 保留（码位体系不能有空洞） |
| 测试支撑（`is_official_client` 等仅测试引用） | 若干 | 保留（有测试价值，删要连带删测试） |
| **真死代码** | — | **已删**：`models/` 模块、`get_provider_config_path`、`OAuthCredentials` 3 字段 + 2 方法 |

---

## 6. 改动前后的自检清单

改任何"app 相关"代码前：

1. 查本文 §1 确认 app 是否在本 fork 范围内。
2. 涉及"当前供应商"→ 查 §3，用 `get_effective_current_provider_with`。
3. 涉及 UI 能力差异 → 查 §2，用对应的 `is*AppId` 而非 `isProxyAppId`。
4. 改文档 → `grep -rn -i "gemini\|opencode\|grok\|openclaw\|hermes\|minimax\|claude-desktop" docs/user-manual/en/`
   剩余命中应仅为：`gemini_native` 协议、Gemini 模型定价表、§3.5 之类的"已移除说明"。

## 7. 相关文档

- `docs/adr/001-fork-charter.md` — fork 章程与季度重审
- `docs/app-removal-plan.md` — 裁剪执行的历史侦察记录（**已过时，仅存档**）
- `docs/fork-patches.md` — 相对上游的补丁清单
