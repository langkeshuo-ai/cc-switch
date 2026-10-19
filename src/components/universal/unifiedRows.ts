import type { AppId } from "@/lib/api";
import type {
  Provider,
  UniversalProvider,
  UniversalProvidersMap,
} from "@/types";
import { APP_IDS } from "@/config/appConfig";
import { extractProviderBaseUrl } from "@/utils/providerConfigUtils";

/** 单个应用的供应商数据（统一视图只读这两样） */
export interface AppProviderData {
  providers: Record<string, Provider>;
  currentProviderId: string;
}

export type AppProviderDataMap = Record<AppId, AppProviderData>;

/** 统一视图的一行：统一供应商，或某个应用自有的供应商 */
export type UnifiedRow =
  | { kind: "universal"; provider: UniversalProvider }
  | { kind: "perApp"; app: AppId; provider: Provider };

/** 统一供应商在应用里的子供应商 id 规则（与后端 to_*_provider 一致） */
export function universalChildId(app: AppId, universalId: string): string {
  return `universal-${app}-${universalId}`;
}

/**
 * 合成统一列表：统一供应商在前（按创建时间、同时间按名称），
 * 各应用自有供应商在后（应用顺序固定）；统一供应商生成的子供应商不单列。
 */
export function buildUnifiedRows(
  universal: UniversalProvidersMap,
  appData: AppProviderDataMap,
): UnifiedRow[] {
  const rows: UnifiedRow[] = [];

  const universals = Object.values(universal).sort(
    (a, b) =>
      (a.createdAt ?? 0) - (b.createdAt ?? 0) || a.name.localeCompare(b.name),
  );
  for (const provider of universals) {
    rows.push({ kind: "universal", provider });
  }

  for (const app of APP_IDS) {
    for (const provider of Object.values(appData[app].providers)) {
      // id 带本应用的 universal 前缀即为统一供应商的子供应商，不单列
      if (provider.id.startsWith(`universal-${app}-`)) {
        continue;
      }
      rows.push({ kind: "perApp", app, provider });
    }
  }

  return rows;
}

/** 按名称 / API 地址过滤（不区分大小写）；空查询返回全部 */
export function filterUnifiedRows(
  rows: UnifiedRow[],
  query: string,
): UnifiedRow[] {
  const q = query.trim().toLowerCase();
  if (!q) return rows;
  return rows.filter((row) =>
    row.kind === "universal"
      ? row.provider.name.toLowerCase().includes(q) ||
        row.provider.baseUrl.toLowerCase().includes(q)
      : row.provider.name.toLowerCase().includes(q) ||
        (extractProviderBaseUrl(row.provider.settingsConfig) ?? "")
          .toLowerCase()
          .includes(q),
  );
}

/** 该统一供应商当前正被哪些应用使用（其子供应商是应用当前供应商） */
export function currentAppsOf(
  provider: UniversalProvider,
  appData: AppProviderDataMap,
): AppId[] {
  const out: AppId[] = [];
  if (
    provider.apps.claude &&
    appData.claude.currentProviderId === universalChildId("claude", provider.id)
  ) {
    out.push("claude");
  }
  if (
    provider.apps.codex &&
    appData.codex.currentProviderId === universalChildId("codex", provider.id)
  ) {
    out.push("codex");
  }
  return out;
}
