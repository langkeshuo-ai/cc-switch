# 开发交接文档（换机冷启动入口）

> 更新：2026-10-19 ｜ 适用：任何 agent 在新机器上接手本仓库开发
> 读完本文即可开工：项目定位 → 仓库现状 → 待办 → 环境 → 凭据 → 闸门 → 坑位 → 文档索引。

## 1. 这是什么项目（30 秒版）

- `langkeshuo-ai/cc-switch`：`farion1231/cc-switch` 的**个人 fork**（Tauri 2 + Rust + React）。
- 用途：Claude Code / Codex / Pi 三应用的供应商切换 + 本地代理路由（127.0.0.1:15721）。
- fork 自研（fork 留存价值，上游/魔改版都没有）：**key 池八道闸 failover、用量统计、快照回滚、云同步（WebDAV/S3 + E2E 加密）、三平台自动更新链路**。
- 上游同步策略：**月度 cherry-pick**（见 `docs/adr/001-fork-charter.md`），不做 wholesale merge。
- 与 magpie 的关系：对比结论见 `docs/compare-magpie-20261010.md`——可共存，fork 留存价值不变。

## 2. 仓库现状（2026-10-19）

| 对象 | 状态 |
|---|---|
| `origin/main` | `0355d914`（v4.0.16 升版基线 + 工作纪要），**v4.0.16 已升版、闸门全绿，未发布** |
| tag `v4.0.15` | `d690f29f`，已发布（2026-10-18，三平台签名产物齐全） |
| 分支 `feat/unified-providers-routing` | `9ac78d82`：**统一供应商页 + 应用路由统一开关**（19 文件 +1960/-23，闸门全绿，**未合并、未发布**，已推远端） |
| 远端 release 遗留 | `v4.0.13` 旧 draft（低优先级，可删） |
| 本地历史分支 | `feat/profile-snapshots`、`feat/toml-surgical`、`fix/*`、`trim/*`、`aps/*`、`upgrade/v4.0.6` 等——历史工作分支，内容已入 main，**不用管，别清理** |

v4.0.16 升版位置：`package.json` / `src-tauri/tauri.conf.json` / `src-tauri/Cargo.toml`(+lock) / whats-new JSON（文件名 = 版本号，top-level `version` 字段必须匹配；条目 ≤4、type ∈ {new, fix, improve}、zh≤40 / zh-TW≤40 / ja≤50 / en≤100 字符，校验器会查）。

## 3. 待办（按优先级）

1. **功能分支真机冒烟**（合并前必做）：统一页增 / 改 / 删 + 级联删除被使用时拒绝 + 路由开（确认框、服务未跑自动启动）/ 关（撤销 toast）+ 路由/聚合子模式 + 供应商下拉（直连 / 路由两态）+ pi 默认切换 + 应用页「统一」徽标。
2. **v4.0.16 正式发布**（阻塞在 6 个 Apple secrets，见 §5）：补齐后先 workflow_dispatch 干跑 `release.yml`（只验签名/公证链路、不建 release），通过再推 tag `v4.0.16`。
3. **功能分支合并时机**：进 v4.0.16 还是留待下版，冒烟通过后定；一条 `git merge feat/unified-providers-routing` 或走 PR。
4. 可选：给 minisign 签名密钥加密码（当前无密码）；删远端 v4.0.13 draft release。
5. 例行：月度上游 cherry-pick（ADR-001）。

## 4. 新机器环境搭建

```bash
# Node 22.12.0（仓库根 .node-version）+ pnpm
node -v && pnpm -v
# Rust：rust-toolchain.toml 钉 channel = "1.95"
rustup toolchain install 1.95          # 必须显式装，别靠工具链文件自动拉
# （macOS 构建才需要）rustup target add x86_64-apple-darwin aarch64-apple-darwin --toolchain 1.95
pnpm install
# 验证：跑 §6 全闸门，绿即环境就绪
```

- 本机 Clash 系统代理会让 reqwest 默认 client 返回 502——测试里已有的 `no_proxy()` 处理沿用即可，别删。
- 远端状态核实一律走 `gh api`（git 直连 443 间歇不稳）。

## 5. 凭据迁移清单（换机关键）

**已在 fork GH secrets 里**：`TAURI_SIGNING_PRIVATE_KEY`（minisign 私钥，**无密码**）。旧机本地备份在 `C:/Users/C/WorkBuddy/cc/.workbuddy/secrets/tauri-updater.key`（+ `.pub`）。如需在新机重新注入：`gh secret set TAURI_SIGNING_PRIVATE_KEY --repo langkeshuo-ai/cc-switch < 密钥文件`（值走 stdin，不落命令行/日志/git）。

**待创建（v4.0.16 macOS 签名/公证，6 个）**：

| Secret | 内容 | 来源 |
|---|---|---|
| `APPLE_CERTIFICATE` | Developer ID Application 证书 .p12 的 base64 单行 | Xcode 建证书 → 钥匙串导出 .p12 → `base64 -i 证书.p12 \| tr -d '\n'` |
| `APPLE_CERTIFICATE_PASSWORD` | 导出 .p12 时设的密码 | 自定 |
| `KEYCHAIN_PASSWORD` | CI 临时钥匙串密码 | 自定强密码 |
| `APPLE_ID` | 开发者账号邮箱 | — |
| `APPLE_PASSWORD` | 该 Apple ID 的**专用密码**（非账号密码） | appleid.apple.com → 登录与安全 → 专用密码 |
| `APPLE_TEAM_ID` | Team ID | developer.apple.com → Account → Membership Details |

前置：Apple Developer 账号（$99/年）+ 已接受最新版 Developer Program License（未接受时公证报 "A required agreement is missing or has expired"，只能人工）+ 一台有完整 Xcode 的 Mac 建证书。注入一律 `gh secret set <名> --repo langkeshuo-ai/cc-switch < 值文件`，凭据值不进命令行 / 文件 / 日志 / git。

**GH 登录**：新机 `gh auth login`，token 需 repo + workflow 权限。

## 6. 闸门（交付前必须全绿）

```bash
cd src-tauri
cargo fmt --all --check
cargo clippy --lib
cargo test --lib -- --test-threads=1     # 必须单线程（测试有共享状态）
cd ..
pnpm typecheck
pnpm format:check
pnpm test:unit                           # 全量约 11-13 分钟，后台跑
```

当前基线（2026-10-19，含功能分支代码）：Rust 3205 passed / 0 failed / 6 ignored；前端 185 文件 / 2162 条 passed。

测试基建约定：
- Tauri `invoke` 被全局 MSW 打桩 → `http://tauri.local/<command>`（`tests/msw/tauriMocks.ts` + `tests/msw/handlers.ts` 默认桩 + `tests/msw/state.ts` 共享状态），用例内 `server.use(...)` 覆盖。
- 组件测试 i18n 约定：`vi.mock("react-i18next", () => ({ useTranslation: () => ({ t: (k) => k }) }))`，断言直接对 key 字符串。
- 新功能惯例：决策逻辑先抽纯函数（参照 `src/components/universal/unifiedRows.ts` / `unifiedRouting.ts`）配单测；交互路径写组件测试（参照 `tests/components/AppRoutingPanel.test.tsx` 9 条路径写法）。

## 7. 坑位清单（真实付过代价的经验）

**构建 / 发布**
- tauri CLI 非交互挂起：`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 缺失时 CLI 在密钥加载处等交互输入（**无密码密钥也会触发**）→ 必须显式 `-p ""` 或导出空环境变量。
- pnpm 脚本 cwd 是 package.json 所在目录（仓库根），不是 src-tauri → `pnpm tauri signer sign` 的相对路径会解析到根目录 target 下（os error 3），**一律绝对路径**。
- tauri `productName` 是 `CC Switch`（带空格）→ dmg 实际文件名 `CC Switch_4.0.15_universal.dmg`。
- updater 端点只留 fork 的 `releases/latest/download/latest.json`（上游路径已切断，别改回去）；`release.yml` 的 prerelease 必须是 false（否则 `releases/latest` 不指向 pre-release，端点 404）。
- fork 的 `release.yml` 在 Apple 凭据就位前，每次 tag push 都会因缺签名键失败——**预期行为**，别当事故。
- 验签：tauri signer 只有 sign/generate 无 verify；用 `scripts/verify-tauri-sig.mjs`（minisign 格式：公钥 42B = 2B 算法 + 8B key_id + 32B ed25519 公钥；签名 bin1 74B + 末行 64B 全局签名；`ED`(0x45,0x44) 算法对 blake2b(文件字节) 验签）。

**网络 / 下载**
- 33MB 级大文件下载：后台 `bash_start` + 耐心轮询可成（约 40 分钟）；前台 300s 超时**不等于**永久失败，别推成结论。
- GitHub 443 直连（git 通道）间歇断连；API 通道（`gh api`）稳定 → 远端状态核实走 API。

**已知差异 / 已知问题（别当 bug 修）**
- 上游 open issue #8273 影响 fork；#8298 已修复（commit `1a0916ef`）。
- 上游的压缩特性（`8641cd1d`）与首字时间特性（`1266e838`）fork 中**不存在**，是已知差异。
- `tool_version.rs` 有两个上游既有 warning（unused variable / dead_code），不动。
- fork `ProxyError::UpstreamError` 比上游多 `retry_after` 字段；`settings.rs` 含 H2 DPAPI 凭据加密层（Windows 特有）。
- 前端 `UniversalProviderApps` 有 `pi` 字段、Rust 端是 `gemini` 字段——两端不对齐，serde default + 忽略未知字段使运行时无害（已知债，未修）。
- 本机 CC Switch 运行时 provider 漂移 + cindy-rewrite 15722 DOWN → dev gate 自检 FAIL 属环境漂移，非代码交付面问题。
- 组件测试止步 jsdom：jsdom 与真实 WebKit 的渲染差异是项目级基线风险，新功能合并前人工真机冒烟一遍即可。

## 8. 关键文档索引

| 文档 | 内容 |
|---|---|
| `docs/handoff.md` | 本文（交接 / 现状 / 待办 / 环境 / 凭据 / 坑位） |
| `docs/unified-providers-routing-20261019.md` | 统一供应商页 + 应用路由功能纪要 |
| `docs/compare-magpie-20261010.md` | magpie 对比与 fork 留存价值论证 |
| `docs/adr/001-fork-charter.md` | fork 章程 + 月度 cherry-pick 策略 |
| `docs/ssot.md` / `docs/fork-patches.md` | 事实源约定 / fork 补丁清单 |
| `BASELINE.md` | 基线说明 |
| `.github/workflows/release.yml` | 三平台签名发布流（macOS 干跑姿势见 §3） |
| `scripts/verify-tauri-sig.mjs` | updater 签名验签脚本 |

## 9. 工作铁律（用户硬性约束，浓缩）

- 不保留向后兼容，过时直接删（不加兼容层 / migration / fallback）；最简实现，不做预防性抽象。
- 分层系统先跑通最小端到端再往上加。
- 所有用户可见输出 / 文档 / 注释 / commit message 一律**简体中文**；注释说「为什么」。
- 变更边界：每处改动直接追溯当前需求，禁止顺手改相邻代码 / 格式 / 命名。
- commit 格式：`类型(范围): 简短中文标题` + 空行 + `-` 开头中文要点；授权提交时 `git add .` 全量提交；带 trailer `Co-Authored-By: <agent> (<model>) <邮箱>`。
- 无验证不交付：「已完成」必须附命令 + 真实输出 + exit code；修 bug 先写复现测试。
- 禁止交互性测试（开浏览器 / 点页面 / 填表 / 人工冒烟由用户做）；验证只用非交互方式。
- 严禁硬编码敏感信息；凭据只走 GH secret / 凭据管理器，值不进命令行 / 文件 / 日志 / git。
- 未明确授权禁止 commit / push / rebase / reset / checkout --（丢弃未提交工作绝对禁止）。
