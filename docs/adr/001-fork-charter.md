# ADR 001：fork 宪法 —— cc-switch 裁剪分支的根本目标与架构边界

- 状态：**Accepted（2026-09-29 陈工拍板）** — 由 2026-09-29 对抗式审计 P-B1 提出
- 下次重审：**2026-12-29**（季度评审；到期须重新确认"永久分叉 + 选择性同步"是否仍然成立）
- 背景：上游 farion1231/cc-switch 于 2026-09 下旬完成"key-field 写引擎"重构
  （`81df5a08`），并以 `15c0b3ce` "per-app direct/proxy mode" 替代了 live 文件
  备份/恢复体系。本 fork 的核心架构与上游正面分叉，必须书面回答
  "我们为什么存在"。

## 决策草案

**fork 的根本目标集合（按优先级）：**

1. **本地网关中间人能力**：代理接管 + 故障转移 + 会话粘性 + 用量统计/计费。
   这是 backup→rewrite→byte-exact restore 体系存在的唯一理由——流量必须过
   网关，接管必须可逆。
2. **三应用裁剪**：只管理 claude / codex / pi，删除其余应用的维护成本。
3. **pi 深度接管**：以 settings.json defaultProvider 为唯一权威源的 Pi 接管
   （上游没有的能力）。

**推论（若本 ADR 被批准为 Accepted）：**

- backup/restore 双路径、strict/best_effort、OAuth 快照三件套、熔断器锁序
  不变量是**核心资产**，任何简化不得触碰（见审计报告墙清单 W1-W7）。
- 上游的 key-field 引擎与 direct/proxy 模式**不采纳**（它们放弃中间人能力
  = 放弃目标 1）；同步机制按"分类 cherry-pick"执行（见审计报告第三部分）。
- skill 系统（services/skill.rs 6314 行）与 usage_stats 的存留需在同一决策
  中表态：建议 usage 保留（目标 1 的一部分）、skill 保留但冻结新功能。

**若被否决（跟随上游架构）：** 删除 proxy.rs 约一半代码，接受一次性 L 级
重写，换取与上游合流。此前所有接管相关测试（约 40 个）随之废弃。

## 后果

- ~~否决：接受一次性重写成本；fork 的 22 个领先提交大部分作废。~~
- **已裁决（2026-09-29）：Accepted。** 保留中间人架构；上游同步按"分类
  cherry-pick + 季度评审"执行（见 `docs/fork-patches.md` 与
  `scripts/upstream-review.sh`）；每季度重审本 ADR 一次。
