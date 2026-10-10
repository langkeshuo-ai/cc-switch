# CC Switch（个人裁剪版）vs Magpie 对比

> 日期：2026-10-10 ｜ 对比对象：`langkeshuo-ai/cc-switch` v3.20.4-trim.13 vs `yetone/magpie`（2026-09-23 发布，MIT，~4.5k stars）
> 结论先行：**两者定位不同、可共存**；magpie 没有用量统计/快照/云同步/failover——这四项恰是本 fork 的留存价值。

## 一、一句话定位

- **CC Switch**：供应商/配置切换器 + 本地代理接管 + **用量统计 + 快照回滚 + 云同步 + failover** 的桌面管理台。
- **magpie**：菜单栏「每 agent 用什么模型」切换器 + **跨 API 翻译网关**（OpenAI Chat/Responses ⇄ Anthropic Messages ⇄ Gemini，含流式/工具调用/推理）。

## 二、逐项对比

| 维度 | CC Switch（trim.13） | magpie |
|---|---|---|
| Agent 覆盖 | Claude / Codex / Pi（裁剪后 3 个；上游 7 个） | 24 个 agent + 任意「带 baseUrl 的东西」 |
| 本地网关 | 127.0.0.1:15721，claude/codex 接管转发（不做 API 格式翻译） | 127.0.0.1:3425，四协议互译，流式/工具/推理全支持 |
| 配置写入 | 外科手术式合并引擎（保留用户写入 key、剥离占位符） | 外科手术式编辑（保留注释/顺序/缩进）+ 原子写 |
| 订阅共享 | ❌（订阅桥接在 DSH 插件层做） | ✅ 订阅即 provider，多账号轮换 **账号级 failover**；Claude 特例驱动真 claude 二进制规避第三方判定 |
| 用量统计 | ✅ usage_daily_rollups + dashboard | ❌ 未提供 |
| 快照/回滚 | ✅ app snapshots（T2 后存 settings） | ❌ |
| 云同步 | ✅ WebDAV/S3 + 可选 E2E 加密 + 完整性校验 | ❌ |
| failover | ✅ per-app failover | 仅账号级（同订阅多账号轮换） |
| 技术栈 | Tauri 2 + Rust + React | Go + Wails，< 15MB，另有 TUI/纯 CLI |
| 平台 | Win/macOS/Linux，NSIS 安装器 | Win/macOS/Linux；**Windows/Linux 构建未签名** |
| 维护模型 | 个人 fork，月度 ADR-001 pick 上游 | yetone（avante.nvim 作者）高频活跃，日增 ~1.3k stars |
| 许可 | MIT | MIT |

## 三、风险评估（magpie 侧）

1. **订阅共享踩 ToS**：官方 README 自己警告 Anthropic 会把其他 agent 的流量判为第三方（magpie 用驱动真 claude 二进制绕过），Google 可能封 Antigravity 账号。与你在 DSH 插件层的桥接思路同源，风险自担。
2. **年轻项目**（发布仅两周）：24 个 agent 的配置格式都可能漂移；Windows 构建未签名。
3. **密钥保管**：第三方中转密钥交 magpie 本地保管，务必从官方 Releases 下载。

## 四、对本 fork 的决策含义

- **可共存**：端口不同、职责不同。magpie 补的正是本 fork 刻意裁掉的区域（跨 agent、跨 API 翻译）。
- **fork 留存理由不变**：用量统计、快照回滚、云同步、failover 四项 magpie 均无——T1–T5 维护工程投入仍然成立。
- **减法机会（备用）**：若某天 magpie 稳定且满足切换需求，fork 可进一步收缩到「用量 + 同步 + failover」纯管理台，连代理接管都可再议——但那是另一个 ADR，今天不动。
- 对「是否升 4.0」无影响。

## 五、建议

1. 短期：观察 magpie（两周龄），Windows 出签名版、agent 配置格式稳定后再试装，与 CC Switch 并行跑不冲突。
2. 订阅共享功能**不要**接主力 Claude/ChatGPT 账号（与 DSH 桥接相同的封号风险敞口不叠加）。
3. 本 fork 按 4.0 升级计划继续推进（另见 `upgrade-eval-v4.0.6-20261010.md`）。
