# PI-ROUTING.md — Pi 的路由方式（本 fork 不实现本地路由）

## 结论

**本 fork 不为 Pi 实现任何本地代理路由。** Pi 的请求路径完全由 Pi CLI 自身决定，
依据 `~/.pi/agent/models.json` 中各供应商条目的 `baseUrl`。

## Pi 如何接入自定义网关

Pi CLI 的供应商模型条目（`models.json`）里，每个条目自带 `baseUrl`。要让某个
供应商走中转/网关，直接把该条目的 `baseUrl` 写成网关地址即可——这与在
Claude Code / Codex 里填第三方 BASE_URL 是同一种用法，区别仅在配置文件归属：

- Pi 拥有 `~/.pi/agent/settings.json` 的账号登录与 `defaultProvider`（当前生效
  供应商/模型）。
- CC Switch 只管理 `models.json` 中的**显式供应商条目**（增删改、切换时补写），
  不拥有 Pi 的登录态，不拥有 `defaultProvider` 的权威值。

上游 PR #6064 的设计边界（本 fork 完整继承）：
"Keep the feature boundary intentionally small: no Pi gateway, routing,
failover, MCP, plugin, theme, or CC Switch-owned default-model behavior."

## 历史说明

fork 曾在 v3.20.4-trim.6 ～ trim.13 期间实现过 Pi 本地代理层（`/pi/anthropic`、
`/pi/openai` 网关路由 + models.json baseUrl 接管），已于 trim.14 起全部移除，
原因见 `BASELINE.md` 第 2 节（schema 对齐上游）与仓库提交历史。
若需要 Pi 走本地网关，请在 `models.json` 的对应条目里手写网关 `baseUrl`，
或使用上游官方后续提供的方案。
