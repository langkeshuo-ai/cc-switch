# 统一供应商页 + 应用路由统一开关 —— 工作纪要

> 日期：2026-10-18 ～ 2026-10-19 ｜ 缓存分支：`feat/unified-providers-routing` @ `9ac78d82`（已推 origin，未发布）｜ main 保持 `26768620`（v4.0.16 发布基线）
> 结论先行：magpie 式「全部供应商」全局页与「应用路由」每应用统一开关两个功能落地；全闸门绿（前端 185 文件 / 2162 条、cargo 3205 条）；工作已缓存进仓库功能分支，发布线未动。

## 一、背景与方向判断

用户质疑：「是不是有点反复造轮子，magpie 更好用，直接安装就可以了。」

基于 2026-10-10 对比文档（`compare-magpie-20261010.md`）的判断：

- 对一半：「统一供应商列表 + 加一次处处用」这个体验是 magpie 的原生强项，在 cc-switch 内做确实是重做该体验。
- 不构成完全替代：fork 的留存价值——key 池八道闸 failover、用量统计、快照回滚、云同步——magpie 均无。
- 两者可共存不冲突（本地端口 15721 vs 3425，职责不同）。
- 用户决策：保留功能、继续用 cc-switch（magpie 需要时可另装并行）。

## 二、功能一：统一供应商页（allProviders）

- 新增「全部供应商」全局页（侧栏全局区，Layers 图标），单列表 = 统一供应商（一卡标注覆盖应用 + 使用中徽标）+ 各应用自有供应商（应用徽标 + 使用中徽标），支持按名称 / API 地址搜索。
- 后端语义修正（`commands/provider.rs`、`services/provider/mod.rs`）：
  - `upsert_universal_provider` 保存后自动同步——编辑一次即全应用生效，取消手动同步步骤；
  - `delete_universal_provider` 级联删除各应用子供应商；任一子供应商正被使用（本地指针 / DB 当前 / 代理路由）时拒绝删除并指明应用。
- 应用页对 universal 子供应商加「统一」徽标（悬停说明：此处编辑只影响本应用）。
- 纯函数模块 `unifiedRows.ts`（列表合成 / 过滤 / 使用中判定）+ 12 条单测。

## 三、功能二：应用路由统一开关

统一页顶部「应用路由」区块，三张卡：

- Claude / Codex：
  - 路由开关：开 = 进路由模式（确认框写明后果与路由目标；服务未跑先自动启动，启动失败不进模式）；关 = 回直连（带「撤销」toast）。
  - 路由 / 聚合子模式切换（两应用均支持聚合）。
  - 生效供应商下拉：后端 switch 模式自适应（路由模式只换路由，直连模式写客户端文件）；既有防护（需路由提示、官方账号拦截）全部继承。
  - 路由模式 + 服务掉线时卡内联提示 + 「启动服务」按钮。
- Pi：无路由概念，默认供应商下拉 + 已启用家数。
- 动作全部复用现有查询与钩子（`useAppMode` / `useModeActions` / `switchProvider` / `usePiCurrentState`），未新增后端命令。
- 关键契约已核实：后端 `current` 在代理模式下返回路由到的那家——下拉预选与「使用中」判定天然一致。
- 纯函数模块 `unifiedRouting.ts`（路由开关判定 / 进入路由目标 / 每应用路由模型）+ 16 条单测。
- 组件顺手修正：两个路由开关的 aria-label 由同名改为带应用名（可测性 + 无障碍）。

## 四、剩余风险处理

初版交付的剩余风险：面板 9 条交互路径只过了类型 / 纯函数闸门，从未被执行。处理方式：`tests/components/AppRoutingPanel.test.tsx`（9 条），走项目 MSW 基建（Tauri invoke → `http://tauri.local/<command>` 打桩），对发出的命令参数做逐字段断言：

1. 三卡渲染（模式徽标 + 生效供应商）
2. 直连开路由：确认 → `set_proxy_takeover_for_app {enabled:true, stack:false, route:当前}`，不误触发启动
3. 服务未跑开路由：先 `start_proxy_server` 再进模式
4. 关路由：无确认框，`{enabled:false, route:null}`
5. 路由 → 聚合：`{stack:true, route:路由目标}`
6. 路由模式下拉换商：`onSwitch("claude", provider)`
7. 直连模式下拉换商：`onSwitch("codex", provider)`
8. 服务掉线提示 + 启动按钮：只发 `start_proxy_server`
9. pi 默认切换：`onSwitch("pi", provider)`

结果：9 条交互路径组件行为首跑即全部正确；测试侧首跑 6 条失败均为测试写法问题（within 双重包装、异步数据未等、Radix 同值不触发切换、文本重复匹配）。收敛后剩余风险 = jsdom 与真实 WebKit 的渲染差异（与项目既有全部组件测试同级），逻辑层已钉死。

## 五、闸门记录

| 闸门 | 结果 |
|---|---|
| cargo fmt / clippy | 过 / exit 0 |
| cargo test --lib -- --test-threads=1 | 3205 passed / 0 failed / 6 ignored，exit 0 |
| pnpm typecheck | 0 错，exit 0 |
| pnpm format:check | All matched files use Prettier code style! |
| pnpm test:unit | 185 文件 / 2162 条 passed，exit 0（含新增 16 + 9 条） |
| GUI 真机冒烟 | 未做（规则禁止交互性测试）；合并前建议手动一遍 |

## 六、关键文件

- `src/components/universal/UnifiedProvidersPage.tsx` 统一供应商页
- `src/components/universal/AppRoutingPanel.tsx` 应用路由面板
- `src/components/universal/unifiedRows.ts` / `unifiedRouting.ts` 纯函数模块（+ 同目录 .test.ts）
- `src-tauri/src/commands/provider.rs` / `src-tauri/src/services/provider/mod.rs` universal 供应商自动同步 + 级联删除
- `src/lib/navigation.ts` / `src/components/shell/Sidebar.tsx` 全局页与侧栏入口
- `src/components/providers/ProviderCard.tsx` universal 子供应商「统一」徽标
- `src/i18n/locales/{zh,zh-TW,ja,en}.json` 四语词条
- `tests/components/AppRoutingPanel.test.tsx` 组件交互测试

## 七、存储与发布线

- 缓存：分支 `feat/unified-providers-routing` @ `9ac78d82`（19 文件，+1960/-23），已推 origin；远端可一键建 PR。
- main 保持 `26768620`，未打 tag、未触发发布。
- 合并方式：`git merge feat/unified-providers-routing`（或走 PR 审查）。

## 八、下一步

1. 合并前真机冒烟：统一页增 / 改 / 删 + 路由开 / 关 + pi 默认切换。
2. v4.0.16 正式发布仍阻塞在 6 个 Apple secrets（见 `.workbuddy/apple-secrets-清单.md`）：补齐后先干跑 macos-release，通过再推 tag。
3. 本功能进 v4.0.16 还是留待下版，合并时定。
4. 可选：给 minisign 签名密钥加密码。

## 九、技术债（本轮未动）

- Rust 端 `UniversalProviderApps` 有 `gemini` 字段、前端类型有 `pi` 字段，两端不对齐；serde default + 忽略未知字段使运行时无害。
