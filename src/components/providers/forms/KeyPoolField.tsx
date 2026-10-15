import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Eye, EyeOff, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { HoverTip } from "@/components/ui/hover-tip";
import { FormLabel } from "@/components/ui/form";
import { ImeSafeInput } from "@/components/ui/ime-safe-input";
import { Input } from "@/components/ui/input";
import type { KeyPoolEntry, KeyPoolStrategy } from "@/types";

/** 表单里的一行：多一个只在前端用的行 id，删行时输入框不串位。 */
export type KeyPoolRow = KeyPoolEntry & { rowId: string };

let nextRowId = 0;

export function createKeyPoolRow(seed?: Partial<KeyPoolEntry>): KeyPoolRow {
  nextRowId += 1;
  return { id: "", key: "", ...seed, rowId: `key-pool-${nextRowId}` };
}

/**
 * 存进 `meta.keyPool` 的样子：去掉空行，按顺序补 `id`（没填 id 时用行序生成稳定
 * 标识——`id` 是冷却与 usage 归属的键，不能为空）。
 *
 * 保留用户显式填的 id（编辑存量池时不改变归属键）。
 */
export function normalizeKeyPool(rows: KeyPoolRow[]): KeyPoolEntry[] {
  const result: KeyPoolEntry[] = [];
  rows.forEach((row, index) => {
    const key = row.key.trim();
    if (!key) return;
    const id = row.id.trim() || `key-${index + 1}`;
    const label = row.label?.trim() || undefined;
    // N2：日限额留空 = 不限（序列化时剥除，与 Rust skip_serializing_if 对齐）
    const dailyLimitUsd = row.dailyLimitUsd?.trim() || undefined;
    // R7：并发上限留空 / 0 / 非数字 = 不限（与 Rust 端同语义）
    const parsedCap = Number.parseInt(String(row.maxConcurrency ?? ""), 10);
    const maxConcurrency =
      Number.isFinite(parsedCap) && parsedCap > 0 ? parsedCap : undefined;
    result.push({
      id,
      key,
      ...(label ? { label } : {}),
      ...(dailyLimitUsd ? { dailyLimitUsd } : {}),
      ...(maxConcurrency ? { maxConcurrency } : {}),
    });
  });
  return result;
}

const STRATEGY_OPTIONS: Array<{
  value: KeyPoolStrategy;
  labelKey: string;
  fallback: string;
}> = [
  {
    value: "order",
    labelKey: "providerForm.keyPoolStrategyOrder",
    fallback: "顺序（故障转移）",
  },
  {
    value: "rotate",
    labelKey: "providerForm.keyPoolStrategyRotate",
    fallback: "轮转（主动分摊）",
  },
  {
    value: "usage",
    labelKey: "providerForm.keyPoolStrategyUsage",
    fallback: "按用量（消耗摊平）",
  },
];

interface KeyPoolFieldProps {
  rows: KeyPoolRow[];
  onRowsChange: (rows: KeyPoolRow[]) => void;
  strategy: KeyPoolStrategy;
  onStrategyChange: (strategy: KeyPoolStrategy) => void;
}

/**
 * Key 池：同一供应商配多把 API key，按策略路由（P-C / R1）。
 *
 * 空池时**整块不渲染**（调用方控制）：没配池的供应商走单 key 路径，
 * 不该在表单里多一块看不懂的东西。
 */
export function KeyPoolField({
  rows,
  onRowsChange,
  strategy,
  onStrategyChange,
}: KeyPoolFieldProps) {
  const { t } = useTranslation();

  const updateRow = (rowId: string, patch: Partial<KeyPoolEntry>) =>
    onRowsChange(
      rows.map((row) => (row.rowId === rowId ? { ...row, ...patch } : row)),
    );

  return (
    <div className="space-y-3">
      <div className="space-y-1">
        <div className="flex items-center justify-between gap-3">
          <FormLabel>
            {t("providerForm.keyPoolLabel", { defaultValue: "Key 池" })}
          </FormLabel>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => onRowsChange([...rows, createKeyPoolRow()])}
            className="h-7 gap-1"
          >
            <Plus className="h-3.5 w-3.5" />
            {t("providerForm.keyPoolAdd", { defaultValue: "添加 Key" })}
          </Button>
        </div>
        <p className="text-xs leading-relaxed text-fg-2">
          {t("providerForm.keyPoolHint", {
            defaultValue:
              "配多把 API key 轮换使用，降低单 key 限流的影响。留空则走单一的 API Key。",
          })}
        </p>
      </div>

      {rows.length > 0 && (
        <div className="space-y-1">
          <FormLabel>
            {t("providerForm.keyPoolStrategyLabel", {
              defaultValue: "路由策略",
            })}
          </FormLabel>
          <div className="flex flex-wrap gap-2">
            {STRATEGY_OPTIONS.map((option) => (
              <Button
                key={option.value}
                type="button"
                variant={strategy === option.value ? "default" : "outline"}
                size="sm"
                onClick={() => onStrategyChange(option.value)}
                aria-pressed={strategy === option.value}
                className="h-7"
              >
                {t(option.labelKey, { defaultValue: option.fallback })}
              </Button>
            ))}
          </div>
          <p className="text-xs leading-relaxed text-fg-2">
            {t("providerForm.keyPoolStrategyHint", {
              defaultValue:
                "顺序：每请求用第一把可用的 key；轮转：依次轮换；按用量：优先用累计消耗最少的 key。",
            })}
          </p>
        </div>
      )}

      {rows.length === 0 ? (
        <p className="text-xs leading-relaxed text-fg-2">
          {t("providerForm.keyPoolEmpty", {
            defaultValue: "未配置 Key 池：请求只使用上面那一个 API Key",
          })}
        </p>
      ) : (
        <div className="space-y-2">
          <div className="hidden grid-cols-[minmax(0,1fr)_minmax(0,1fr)_120px_100px_36px] gap-2 px-1 text-xs font-medium text-fg-2 md:grid">
            <span>
              {t("providerForm.keyPoolKeyLabel", { defaultValue: "API Key" })}
            </span>
            <span>
              {t("providerForm.keyPoolNameLabel", { defaultValue: "备注名" })}
            </span>
            <span>
              {t("providerForm.keyPoolDailyLimitLabel", {
                defaultValue: "日限额 $（可选）",
              })}
            </span>
            <span>
              {t("providerForm.keyPoolMaxConcurrencyLabel", {
                defaultValue: "并发上限（可选）",
              })}
            </span>
            <span />
          </div>
          {rows.map((row) => (
            <KeyPoolRowInputs
              key={row.rowId}
              row={row}
              onPatch={(patch) => updateRow(row.rowId, patch)}
              onRemove={() =>
                onRowsChange(rows.filter((other) => other.rowId !== row.rowId))
              }
            />
          ))}
        </div>
      )}
    </div>
  );
}

/** 一行 key：密码框 + 显示切换（与 ApiKeyInput 一致的脱敏姿态）。 */
function KeyPoolRowInputs({
  row,
  onPatch,
  onRemove,
}: {
  row: KeyPoolRow;
  onPatch: (patch: Partial<KeyPoolEntry>) => void;
  onRemove: () => void;
}) {
  const { t } = useTranslation();
  const [showKey, setShowKey] = useState(false);

  return (
    <div className="grid grid-cols-1 gap-2 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_120px_100px_36px]">
      <div className="relative">
        <Input
          type={showKey ? "text" : "password"}
          value={row.key}
          onChange={(event) => onPatch({ key: event.target.value })}
          placeholder={t("providerForm.keyPoolKeyPlaceholder", {
            defaultValue: "sk-...",
          })}
          aria-label={t("providerForm.keyPoolKeyLabel", {
            defaultValue: "API Key",
          })}
          autoComplete="off"
          className="pe-10"
        />
        {row.key && (
          <button
            type="button"
            onClick={() => setShowKey(!showKey)}
            className="absolute inset-y-0 end-0 flex items-center pe-3 text-fg-3 transition-colors hover:text-fg-1"
            aria-label={showKey ? t("apiKeyInput.hide") : t("apiKeyInput.show")}
          >
            {showKey ? <EyeOff size={16} /> : <Eye size={16} />}
          </button>
        )}
      </div>
      <ImeSafeInput
        value={row.label ?? ""}
        onValueChange={(value) => onPatch({ label: value })}
        placeholder={t("providerForm.keyPoolNamePlaceholder", {
          defaultValue: "例如：备用 key 1",
        })}
        aria-label={t("providerForm.keyPoolNameLabel", {
          defaultValue: "备注名",
        })}
        autoComplete="off"
      />
      <Input
        type="text"
        inputMode="decimal"
        value={row.dailyLimitUsd ?? ""}
        onChange={(event) => onPatch({ dailyLimitUsd: event.target.value })}
        placeholder={t("providerForm.keyPoolDailyLimitPlaceholder", {
          defaultValue: "如 5 = 每日 $5",
        })}
        aria-label={t("providerForm.keyPoolDailyLimitLabel", {
          defaultValue: "日限额 $（可选）",
        })}
        autoComplete="off"
      />
      <Input
        type="text"
        inputMode="numeric"
        value={row.maxConcurrency?.toString() ?? ""}
        onChange={(event) => {
          const parsed = Number.parseInt(event.target.value, 10);
          onPatch({
            maxConcurrency: Number.isFinite(parsed) ? parsed : undefined,
          });
        }}
        placeholder={t("providerForm.keyPoolMaxConcurrencyPlaceholder", {
          defaultValue: "如 3",
        })}
        aria-label={t("providerForm.keyPoolMaxConcurrencyLabel", {
          defaultValue: "并发上限（可选）",
        })}
        autoComplete="off"
      />
      <HoverTip content={t("common.delete")}>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          onClick={onRemove}
          aria-label={t("common.delete")}
          className="h-9 w-9 text-fg-2 hover:text-destructive"
        >
          <Trash2 className="h-4 w-4" />
        </Button>
      </HoverTip>
    </div>
  );
}
