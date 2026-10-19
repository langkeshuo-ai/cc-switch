import { describe, expect, it } from "vitest";
import type {
  Provider,
  UniversalProvider,
  UniversalProvidersMap,
} from "@/types";
import {
  buildUnifiedRows,
  currentAppsOf,
  filterUnifiedRows,
  universalChildId,
  type AppProviderDataMap,
  type UnifiedRow,
} from "./unifiedRows";

const provider = (
  id: string,
  name: string,
  settingsConfig?: unknown,
): Provider => ({ id, name, settingsConfig }) as Provider;

const universalProvider = (
  id: string,
  name: string,
  createdAt: number,
  apps?: { claude?: boolean; codex?: boolean },
  baseUrl = "https://gateway.example/v1",
): UniversalProvider =>
  ({
    id,
    name,
    providerType: "newapi",
    apps: {
      claude: apps?.claude ?? true,
      codex: apps?.codex ?? true,
      pi: false,
    },
    baseUrl,
    apiKey: "sk-test",
    models: {},
    createdAt,
  }) as UniversalProvider;

const emptyApps = (): AppProviderDataMap => ({
  claude: { providers: {}, currentProviderId: "" },
  codex: { providers: {}, currentProviderId: "" },
  pi: { providers: {}, currentProviderId: "" },
});

const describeRow = (row: UnifiedRow): string =>
  row.kind === "universal"
    ? `u:${row.provider.id}`
    : `${row.app}:${row.provider.id}`;

describe("universalChildId", () => {
  it("与后端子供应商 id 约定一致", () => {
    expect(universalChildId("claude", "u1")).toBe("universal-claude-u1");
    expect(universalChildId("codex", "u1")).toBe("universal-codex-u1");
  });
});

describe("buildUnifiedRows", () => {
  it("统一供应商在前（按创建时间，同时间按名称），自有供应商在后，子供应商不单列", () => {
    const universal: UniversalProvidersMap = {
      "u-later": universalProvider("u-later", "Later", 200),
      "u-earlier": universalProvider("u-earlier", "Earlier", 100),
    };
    const appData = emptyApps();
    appData.claude.providers = {
      c1: provider("c1", "Claude Own"),
      "universal-claude-u-earlier": provider(
        "universal-claude-u-earlier",
        "Earlier",
      ),
    };
    appData.codex.providers = {
      "universal-codex-u-earlier": provider(
        "universal-codex-u-earlier",
        "Earlier",
      ),
      k1: provider("k1", "Codex Own"),
    };
    appData.pi.providers = { p1: provider("p1", "Pi Own") };

    const rows = buildUnifiedRows(universal, appData);
    expect(rows.map(describeRow)).toEqual([
      "u:u-earlier",
      "u:u-later",
      "claude:c1",
      "codex:k1",
      "pi:p1",
    ]);
  });

  it("创建时间相同按名称排序", () => {
    const universal: UniversalProvidersMap = {
      "u-b": universalProvider("u-b", "Beta", 100),
      "u-a": universalProvider("u-a", "Alpha", 100),
    };
    const rows = buildUnifiedRows(universal, emptyApps());
    expect(rows.map(describeRow)).toEqual(["u:u-a", "u:u-b"]);
  });

  it("两边都没有供应商时为空列表", () => {
    expect(buildUnifiedRows({}, emptyApps())).toEqual([]);
  });
});

describe("filterUnifiedRows", () => {
  const universal: UniversalProvidersMap = {
    u1: universalProvider("u1", "Acme Gateway", 100),
  };
  const appData = emptyApps();
  appData.claude.providers = {
    c1: provider("c1", "Beta", {
      env: { ANTHROPIC_BASE_URL: "https://beta.example/v1" },
    }),
  };
  const rows = buildUnifiedRows(universal, appData);

  it("空查询或纯空白返回全部", () => {
    expect(filterUnifiedRows(rows, "  ")).toHaveLength(rows.length);
    expect(filterUnifiedRows(rows, "")).toHaveLength(rows.length);
  });

  it("名称不区分大小写匹配", () => {
    expect(filterUnifiedRows(rows, "acme").map(describeRow)).toEqual(["u:u1"]);
  });

  it("统一供应商按 baseUrl 匹配", () => {
    expect(filterUnifiedRows(rows, "gateway.example").map(describeRow)).toEqual(
      ["u:u1"],
    );
  });

  it("应用自有供应商按配置里的地址匹配", () => {
    expect(filterUnifiedRows(rows, "beta.example").map(describeRow)).toEqual([
      "claude:c1",
    ]);
  });

  it("无匹配返回空", () => {
    expect(filterUnifiedRows(rows, "nope")).toEqual([]);
  });
});

describe("currentAppsOf", () => {
  it("子供应商是某应用当前供应商时标记该应用", () => {
    const p = universalProvider("u1", "Acme", 100);
    const appData = emptyApps();
    appData.claude.currentProviderId = "universal-claude-u1";
    appData.codex.currentProviderId = "universal-codex-u1";
    expect(currentAppsOf(p, appData)).toEqual(["claude", "codex"]);
  });

  it("未启用的应用不标记", () => {
    const p = universalProvider("u1", "Acme", 100, {
      claude: true,
      codex: false,
    });
    const appData = emptyApps();
    appData.codex.currentProviderId = "universal-codex-u1";
    expect(currentAppsOf(p, appData)).toEqual([]);
  });

  it("没有应用正在使用时返回空", () => {
    expect(
      currentAppsOf(universalProvider("u1", "Acme", 100), emptyApps()),
    ).toEqual([]);
  });
});
