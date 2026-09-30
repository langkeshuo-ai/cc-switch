# ProviderForm.tsx 拆分方案（设计稿 · 不落地）

> 状态：**设计待评审**，本轮不动代码。
> 基线：`src/components/providers/forms/ProviderForm.tsx` @ `607a2977`，**1754 行**。
> 行号仅用于本次评审定位；按 ADR-001 / `docs/fork-patches.md` 的登记口径，
> 真正动刀时一律以**符号名**检索（`rg -n "function ProviderFormFull" src`），
> 行号漂移不影响本方案的适用性。

---

## 1. 目标与非目标

**目标（可验收）**

| 指标 | 现状 | 目标 |
| --- | --- | --- |
| `ProviderForm.tsx` 行数 | 1754 | ≤ 300（只留组装） |
| 单函数最长 | `handleSubmit`+`performSubmit` 合计 537 行 | 无函数 > 150 行 |
| 单 hook 最长 | — | 无 hook > 250 行 |
| 对外导出契约 | 4 类符号被 6 处引用 | **零破坏**（全部 re-export 保留） |

**非目标（明确不做）**

- 不改 `ProviderFormData` / `ProviderFormValues` 的字段结构，不动后端契约。
- 不引入 zustand 或任何新状态库（与 `cc-switch-borrow-evaluation` 的排除项一致）。
- 不动 `PiProviderForm.tsx`（2038 行）——它有自己的表单生命周期，另排。
- 不顺手改 UI/交互；阶段 3 只做「搬移 + 组合」，不重排 DOM 结构。

---

## 2. 现状结构盘点

| # | 段落 | 符号锚点 | 行区间 | 行数 | 性质 |
| --- | --- | --- | --- | --- | --- |
| P1 | 头部：imports + 纯函数 + 公开类型 | `getPresetProviderType` / `normalizeCodexCatalogModelsForSave` / `normalizeCodexChatReasoningForSave` / `ProviderFormProps` | 1–216 | 216 | 纯逻辑，零 React 依赖 |
| P2 | 公开入口包装 | `function ProviderForm` | 217–224 | 8 | 仅转发 props |
| P3 | 巨型状态与派生 | `function ProviderFormFull` 前半（`useState`/`useMemo`/`useCallback`/派生布尔） | 225–775 | 551 | 状态聚类 + 派生 |
| P4 | 提交校验 | `const handleSubmit` | 776–1014 | 239 | 纯校验 + 软性问题确认流 |
| P5 | 提交落库 | `const performSubmit` | 1015–1312 | 298 | 组装载荷 + 调 API |
| P6 | 速度测试 / 预设切换 / 错误字段 | `shouldShowSpeedTest` / `speedTestEndpoints` / `handlePresetChange` / `settingsConfigErrorField` | 1313–1442 | 130 | 编排 |
| P7 | JSX | `return (` | 1443–1747 | 305 | 表单骨架 + 两个 ConfirmDialog |
| P8 | 公开类型 | `export type ProviderFormValues` | 1748–1754 | 7 | 契约 |

**对外契约（拆分后必须原样可导入）**

- `ProviderForm`（组件）、`ProviderFormProps`、`ProviderFormValues`
- `normalizeCodexCatalogModelsForSave`
- `LocalProxyRequestOverridesBuildResult`

引用点（拆分时必须逐一回归）：
`AddProviderDialog.tsx`、`EditProviderDialog.tsx`、`PiProviderForm.tsx:38`、
`tests/components/ProviderForm.codexCatalog.test.ts:2`、
`tests/components/ProviderForm.codexManagedAccount.test.tsx:6`、
`tests/components/AddProviderDialog.test.tsx:11`（后两者 `vi.mock` 了整个模块路径——
**模块路径 `@/components/providers/forms/ProviderForm` 绝不能改名**，否则 mock 落空、
测试静默通过假实现）。

**现有回归测试锚点（拆分期间必须全绿，不许改断言）**

- `tests/components/ProviderForm.codexCatalog.test.ts` — catalog models load→save 回环
- `tests/components/ProviderForm.codexManagedAccount.test.tsx` — 提交载荷（providerType / accountId / anthropicAuthField / maxOutputTokens / promptCacheRouting）
- `tests/components/AddProviderDialog.test.tsx`、`EditProviderDialog.test.tsx` — 对话框与表单边界

---

## 3. 拆分顺序与每步回归点

原则：**先搬纯逻辑，再搬最大单体，最后搬 JSX**；每步一个独立 commit，可单独 revert；
步与步之间不许混改（避免「改 A 坏 B」时无法二分）。

### 阶段 0 — 纯函数与类型下沉（零行为变更）

**下沉到** `forms/helpers/providerFormNormalizers.ts`（新）

- `getPresetProviderType`
- `normalizeCodexCatalogModelsForSave`（**从 ProviderForm.tsx 继续 re-export**）
- `normalizeCodexChatReasoningForSave`（模块内私有，同文件）
- `type LocalProxyRequestOverridesBuildResult`

**下沉到** `forms/types.ts`（新）：`ProviderFormProps`、`ProviderFormValues`、`PresetEntry`，
并由 `ProviderForm.tsx` re-export，保证 6 个引用点零改动。

**回归点**

1. `pnpm typecheck` 通过（re-export 生效的机器证据）。
2. `tests/components/ProviderForm.codexCatalog.test.ts` 全绿——它直接 import
   `normalizeCodexCatalogModelsForSave`，是 re-export 是否真正生效的唯一探针。
3. `git diff --stat` 中 `ProviderForm.tsx` 只出现 import/export 行。

### 阶段 1 — 提交管线抽 hook（收益最大、风险最高，单独一步）

**1a. 纯校验抽成函数** → `forms/helpers/validateProviderFormValues.ts`

把 `handleSubmit` 里的 A 类（空模板变量 / 空供应商名 / 空 API Key）与
B 类（OAuth 未登录，token 不存在，保存无意义）判定抽成纯函数：

```ts
type ValidationIssue = { kind: "soft" | "blocking"; code: string; context: Record<string, string> };
export function collectValidationIssues(input: ValidationInput): ValidationIssue[];
```

**1b. 编排抽成 hook** → `forms/hooks/useProviderFormSubmit.ts`

搬 `handleSubmit` + `performSubmit`（537 行 → hook，目标 ≤ 250 行）。
返回：

```ts
{
  submit,                     // 原 handleSubmit
  isConfirmSubmitting,
  softIssues,                 // 非 null 时渲染确认流
  confirmSoftIssues,          // 确认后仍保存
  cancelSoftIssues,
}
```

**参数用单个 deps 对象**（不摊平成 20 个位置参数）——与待办的
「usageKeys 参数对象化」同方向，新增依赖时不必改签名：

```ts
useProviderFormSubmit({
  form, appId, initialData, category, isEditMode,
  claudeState, codexState, pricing, localProxy...,   // 分组传入，不打散
})
```

**必须守住的不变量（搬移时逐条核对）**

- 编辑已有 provider 的提交载荷**不得携带 endpoints 字段**（源码注释 `Existing-provider
  edits never own endpoint membership`，后端会拒绝 endpoint-bearing update payload）。
- `providerType` / GitHub Copilot `accountId` / `anthropicAuthField` /
  `localCodexMaxOutputTokens`（仅 codex+anthropic 且用户显式开启时落库）/
  `promptCacheRouting` 的**条件落库语义逐字保留**。
- 软性问题（A 类）走确认框「仍要保存」；B 类硬阻断，不给确认入口。

**回归点**

1. `tests/components/ProviderForm.codexManagedAccount.test.tsx` 全绿——它是载荷级断言的主探针。
2. **新增** `tests/components/providerFormValidation.test.ts`：A 类 → `soft`、B 类 → `blocking`，
   逐条断言，锁住 A/B 分类不被搬移改错（现有测试覆盖不到纯校验层）。
3. 真机冒烟（L0）：新建 provider 保存；编辑 provider 保存；Codex 官方账号未登录时保存被阻断。
4. 软性问题路径：故意留空模板变量 → 出确认框 → 「仍要保存」→ 落库成功。

### 阶段 2 — 状态聚类（三个 hook）

| hook | 搬移内容 | 锚点 | 预计行数 |
| --- | --- | --- | --- |
| `useProviderPresetState.ts` | `selectedPresetId` / `activePreset` / `presetEntries` / `selectedPresetEntry` / `presetCategoryLabels` / `handlePresetChange` | `useState` 270–278、`useMemo` 651–691、`handlePresetChange` 1352–1430 | ~130 |
| `useCodexAdvancedFormState.ts` | `codexFastMode` / `codexChatReasoning` / `promptCacheRouting` / `customUserAgent` / `localProxyHeadersOverride` / `localProxyBodyOverride` / 全部 `localCodex*` + `handleCodexConfigChange` + `handleCodexApiFormatChange` + 两个 effect | 529–555、574–637、638/647 | ~120 |
| `useProviderPricingState.ts` | `pricingConfig` 及其 setter | 298–311 | ~20 |

**回归点**

1. `ProviderForm.codexCatalog.test.ts`：catalog models 的 load→save 回环（阶段 2 直接触碰 codex 状态）。
2. `ProviderForm.codexManagedAccount.test.tsx`：fast mode / chat reasoning / maxOutputTokens 持久化断言。
3. **新增**：预设切换后表单快照断言（选「自定义」→ 字段清空；选具体预设 → 模板值注入），
   并把重跑 `pnpm test:unit` 的结果作为每步完成的证据。
4. 真机冒烟：切换预设、切 API 格式（claude / codex 各一次）、开合 Codex 高级项后保存。

### 阶段 3 — JSX 三分（只搬移，不重排）

| 新组件 | 内容 | 行区间 | 行数 |
| --- | --- | --- | --- |
| `ProviderConfigEditorSection.tsx` | Codex/Claude 编辑器分派 + `CommonConfigEditor` 弹窗 + `settingsConfigErrorField` | 177–227 | ~50 |
| `ProviderFormActions.tsx` | 提交按钮 + `isSubmitting` 禁用态 | 229–242 | ~14 |
| `ProviderFormDialogs.tsx` | 两个 `ConfirmDialog` | 244–259 | ~16 |

**回归点**

1. `AddProviderDialog.test.tsx` / `EditProviderDialog.test.tsx` 全绿（它们 mock 掉 ProviderForm，
   是「模块路径未变」的机器证据）。
2. 真机冒烟（必须，静态测试覆盖不到 DOM）：新建/编辑对话框能打开、字段渲染完整、
   提交按钮禁用态正确、两个确认框各自能弹能取消。
3. 对比阶段 3 前后的 DOM 结构快照（`innerHTML` 或截图）确认无重排。

### 阶段 4 — 收尾

`ProviderFormFull` 收敛为「组装」：`useMemo(defaultValues)` → `useForm` → 各 hook 调用 →
三分 JSX。目标 ≤ 250 行。此时 `ProviderForm.tsx` 总量应落在 250–300 行。

---

## 4. 风险与对策

| 风险 | 触发点 | 对策 |
| --- | --- | --- |
| **陈旧闭包**（最大风险） | 阶段 1 把 537 行搬进 hook，依赖数组漏项 | hook 入参用 deps 对象；搬移后**逐条对照**原函数的依赖来源；真机冒烟必须覆盖「改 A 字段后立刻保存」 |
| `vi.mock` 路径落空 | 任何改动模块文件名/路径 | 路径 `@/components/providers/forms/ProviderForm` **冻结**；用「修改后测试仍全绿」反证 mock 未落空 |
| 条件落库语义被简化 | 阶段 1b 重排 if 分支 | 每步 `git diff` 人工核对那 5 个字段的落库条件；把断言固定进 `codexManagedAccount` 测试 |
| 阶段过大无法二分 | 一个 commit 混两阶段 | 一阶段一 commit；出问题 `git revert` 单点回退 |
| 拆分本身引入回归 | 全部阶段 | 每阶段结束跑：`pnpm typecheck` + `pnpm test:unit` + 对应真机冒烟；任一步不绿不回退下一步 |

## 5. 建议节奏

阶段 0 + 1 一个会话（收益最大的 537 行），阶段 2 一个会话，阶段 3 一个会话。
**不要在阶段 1 未验证时启动阶段 2**——两个阶段都动 codex 状态，混改会让回归归因失效。
