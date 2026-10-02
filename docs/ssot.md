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

## 4. 本地路由端点

监听 `127.0.0.1:15721`（可配）。已注册的路由（`proxy/server.rs`）：

| 路径 | 用途 |
|------|------|
| `/v1/messages`、`/claude/v1/messages` | Anthropic Messages |
| `/pi/anthropic/*rest` | Pi → Anthropic |
| `/pi/openai/responses` | Pi → OpenAI Responses |
| `/chat/completions`、`/v1/chat/completions` | OpenAI Chat |
| `/models`、`/v1/models` | 模型列表 |
| `/health`、`/status` | 运维 |

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
