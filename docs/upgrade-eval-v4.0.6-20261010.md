# 4.0 升级只读评估报告（T5）

> 日期：2026-10-10 ｜ 方法：`git merge-tree --write-tree HEAD v4.0.6`（纯只读，未改任何代码）
> 前提：本评估基于 T1–T4 完成后的工作树（commit `bca24d91`）。
> **本报告仅是决策依据，是否升级由维护者决定。**

## 1. 量级

| 指标 | 数值 |
|---|---|
| 上游 v3.20.4 → v4.0.6 提交数 | 299 |
| 上游变更文件数 | 913 |
| 上游 schema 版本 | 19 → **20**（v19→v20 = mcp_servers 加 enabled_pi 列） |
| merge-tree 冲突文件总数 | **249** |
| 其中 content 冲突 | 176 |
| 其中 modify/delete 冲突 | 73（63 = fork 删除&上游改；10 = 上游删除&fork 改） |
| 双方可自动合并（Auto-merging 成功） | 236 文件 |

## 2. 冲突分类

### A. 裁剪导致的必然冲突（可脚本化，约 55%）

- 73 个 modify/delete：裁掉的 app 域文件上游仍在改 → **裁决恒为「保持删除」**，trim-rules.json 的删除 globs 直接覆盖。
- 10 个反向 modify/delete：上游 4.0 自己重构删掉了 `ProxyPanel.tsx`、`ProxyTabContent.tsx`、`AppSwitcher.tsx`、`ProviderActions.tsx` 等（fork 仅做过裁剪式小改）→ **裁决恒为「跟随上游删除」**。
- docs/user-manual 54 个 + README 4 个 + .github 6 个 content 冲突：内容多为被裁 app 的介绍段落 → 取上游版后跑 trim 规则即可。

### B. 需逐文件人工处理的代码 content 冲突（约 112 个）

| 区域 | 数量 | 说明 |
|---|---|---|
| src-tauri/src（Rust） | 46 | schema.rs、app_config.rs、services/proxy.rs、proxy/providers/mod.rs 等 |
| 前端组件 | 38 | providers/forms、settings、usage、mcp 等 |
| 前端其它（api/hooks/i18n/types） | 14 | i18n 4 个 locale JSON 是键级冲突，好解 |
| 测试（Rust 9 + 前端 10） | 19 | 多数随实现裁决走 |
| 构建清单（package.json / Cargo / tauri.conf 等） | 5 | 版本号与依赖，机械 |

## 3. 语义分歧点（决定工作量与风险的核心）

1. **schema 兼容性 ✅（本次 T2 的直接收益）**：fork 库已回到 v19 = 上游 v3.20.4 形态，升级 4.0 时上游的 `migrate_v19_to_v20` 迁移链可直接套用，**零库手术**。这是此前「不降回就跟版必改库」的唯一硬障碍，现已排除。
2. **Pi 护栏 ✅ 不冲突**：实测上游 v4.0.6 `PROXY_APP_IDS = ["claude","codex","gemini","grokbuild"]`，proxy_config CHECK 仍是同四值——**上游 4.0 也没有把 pi 做成代理 app**。上游给 Pi 加的只是 MCP 面的 `enabled_pi` 标志（配置管理范畴），不违反「pi 不回 proxy」硬约束。
3. **裁剪面扩大**：4.0 新增/扩展了 openclaw / opencode / mcode / hermes / omo 等 UI 面 → trim-rules.json 需增补删除 globs（脚本已为此设计，属规则增补而非脚本改造）。
4. **引擎方向对立（老问题延续）**：上游 key-field switching 引擎（81df5a08 系列）与 fork 外科手术式合并引擎对立，direct/proxy mode 架构（15c0b3ce）此前已按 ADR-001 拒绝——4.0 相关文件冲突须继续按 `docs/fork-patches.md` 登记表逐条「接受/改写」判定，**不能整目录取上游**。
5. **T13/T16/B9 测试取舍**：`codex_config.rs`、`tests/provider_commands.rs` 等在冲突列表中，此前登记的断言分歧（bearer_token 剥离语义、`[mcp_servers]` 存活语义）会重现，pick 时按登记表处理。

## 4. 结论

**推荐：升到 4.0，但按月度流程分两步走，不一次性硬合。**

理由：
- 唯一的硬障碍（schema 跟版必改库）已被 T2 消除；
- Pi 护栏与上游 4.0 实际方向一致，不存在「被迫接受 Pi 代理」的风险；
- T4 裁剪脚本已就绪，对 v4.0.6 的试跑补丁已产出（98 删除 / 49551 行，`cc-switch-artifacts/trim-v4.0.6.patch`），可先在临时分支 dry-run；
- 299 个提交含大量修复，长期停在 3.20.4 会让 pick 债务持续累积（fork-patches.md 已出现「落后 22」的记录）。

风险与代价（须接受再动手）：
- ~112 个代码 content 冲突需逐文件人工裁决，其中引擎对立区（codex_config、provider 引擎）不能贪快取上游；估计 1–2 个专门会话；
- 裁剪规则需增补一轮（4.0 新 app 面）；
- 升级后 schema 到 20，此后再想「停在旧版」回退成本会重新上升——升了就别回头。

保守替代方案：继续停 3.20.4，仅按月度 cherry-pick 安全/bug 修复。适用于：不需要 4.0 新功能（openclaw 面板、计价重构等）且想最小化维护成本的场景。代价是 pick 债务继续滚。

## 附：本次验收自检（T1–T4）

| # | 检查项 | 结果 |
|---|---|---|
| 1 | `PROXY_APP_IDS = ["claude","codex"]`（appConfig.tsx:28） | ✅ |
| 2 | proxy_config CHECK 为上游四值、不含 'pi'（schema.rs:159,910；schema.rs 中其余 18 处 'pi' 均为 normalize 垫片/不变量测试/注释） | ✅ |
| 3 | `SCHEMA_VERSION: i32 = 19`（database/mod.rs:59） | ✅ |
| 4 | Pi 代理残留扫描：无 PiAdapter//pi/anthropic 路由/pi_proxy_override 残留（tests.rs:195 命中为 URL host 判定测试的输入数据，与路由无关） | ✅ |
| 5 | 全量 `cargo test --lib`：2659 passed / 0 failed / 7 ignored；clippy clean；tsc clean | ✅ |

**遗留提醒**：stash `WIP-before-trim-model-refactor-20261010`（busy_timeout 重构 + sync/docs）尚未恢复；其 `database/mod.rs` 改动与 T2 提交重叠，`git stash pop` 预期冲突，建议逐 hunk 处理。
