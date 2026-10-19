import { describe, expect, it } from "vitest";
import type { Provider } from "@/types";
import type { AppMode, AppModeView } from "@/types/proxy";
import {
  buildAppRoutingModel,
  entryRouteTarget,
  isRoutingOn,
} from "./unifiedRouting";
import type { AppProviderData } from "./unifiedRows";

const provider = (id: string, name: string): Provider =>
  ({ id, name }) as Provider;

const modeView = (
  mode: AppMode,
  routeProviderId: string | null,
  directProviderId: string | null,
): AppModeView => ({
  mode,
  attached: true,
  routeProviderId,
  directProviderId,
});

const appData = (
  providers: Record<string, Provider>,
  currentProviderId: string,
): AppProviderData => ({ providers, currentProviderId });

describe("isRoutingOn", () => {
  it("direct 与 undefined 为关", () => {
    expect(isRoutingOn("direct")).toBe(false);
    expect(isRoutingOn(undefined)).toBe(false);
    expect(isRoutingOn(null)).toBe(false);
  });

  it("route / stack 为开", () => {
    expect(isRoutingOn("route")).toBe(true);
    expect(isRoutingOn("stack")).toBe(true);
  });
});

describe("entryRouteTarget", () => {
  it("优先上次路由的那家", () => {
    expect(
      entryRouteTarget(modeView("direct", "route-a", "direct-b"), "direct-b"),
    ).toBe("route-a");
  });

  it("上次路由为空时退回当前直连", () => {
    expect(
      entryRouteTarget(modeView("direct", null, "direct-b"), "direct-b"),
    ).toBe("direct-b");
  });

  it("两者都没有时返回 null（后端沿用上次的路由）", () => {
    expect(entryRouteTarget(modeView("direct", null, null), "")).toBeNull();
  });

  it("modeView 未加载时用当前直连", () => {
    expect(entryRouteTarget(undefined, "direct-b")).toBe("direct-b");
  });
});

describe("buildAppRoutingModel（claude / codex）", () => {
  const data = appData(
    {
      a: provider("a", "供应商 A"),
      b: provider("b", "供应商 B"),
    },
    "b",
  );

  it("直连：路由关、生效供应商是直连那家", () => {
    const model = buildAppRoutingModel(
      "claude",
      data,
      modeView("direct", "a", "b"),
      null,
    );
    expect(model.isProxy).toBe(true);
    expect(model.mode).toBe("direct");
    expect(model.routingOn).toBe(false);
    expect(model.effectiveProviderId).toBe("b");
    expect(model.effectiveProviderName).toBe("供应商 B");
  });

  it("路由：路由开、生效供应商是路由到的那家（后端 current 已反映）", () => {
    const model = buildAppRoutingModel(
      "codex",
      appData(data.providers, "a"),
      modeView("route", "a", "b"),
      null,
    );
    expect(model.routingOn).toBe(true);
    expect(model.effectiveProviderId).toBe("a");
  });

  it("聚合：路由开", () => {
    const model = buildAppRoutingModel(
      "claude",
      data,
      modeView("stack", "a", "b"),
      null,
    );
    expect(model.mode).toBe("stack");
    expect(model.routingOn).toBe(true);
  });

  it("claude / codex 都支持聚合", () => {
    expect(
      buildAppRoutingModel("claude", data, undefined, null).stackSupported,
    ).toBe(true);
    expect(
      buildAppRoutingModel("codex", data, undefined, null).stackSupported,
    ).toBe(true);
  });

  it("modeView 未加载时按直连处理", () => {
    const model = buildAppRoutingModel("claude", data, undefined, null);
    expect(model.mode).toBe("direct");
    expect(model.routingOn).toBe(false);
  });

  it("生效供应商不存在时名称为 null", () => {
    const model = buildAppRoutingModel(
      "claude",
      appData({}, "ghost"),
      undefined,
      null,
    );
    expect(model.effectiveProviderId).toBe("ghost");
    expect(model.effectiveProviderName).toBeNull();
  });
});

describe("buildAppRoutingModel（pi）", () => {
  const data = appData(
    { p1: provider("p1", "P1"), p2: provider("p2", "P2") },
    "",
  );

  it("无路由概念：isProxy 假、模式 null、路由关、不支持聚合", () => {
    const model = buildAppRoutingModel("pi", data, undefined, "p1");
    expect(model.isProxy).toBe(false);
    expect(model.mode).toBeNull();
    expect(model.routingOn).toBe(false);
    expect(model.stackSupported).toBe(false);
  });

  it("生效供应商取默认那家，不是 currentProviderId", () => {
    const model = buildAppRoutingModel("pi", data, undefined, "p2");
    expect(model.effectiveProviderId).toBe("p2");
    expect(model.effectiveProviderName).toBe("P2");
  });

  it("没有默认供应商时生效供应商为 null", () => {
    const model = buildAppRoutingModel("pi", data, undefined, null);
    expect(model.effectiveProviderId).toBeNull();
    expect(model.effectiveProviderName).toBeNull();
  });

  it("providers 列出全部 pi 供应商", () => {
    const model = buildAppRoutingModel("pi", data, undefined, null);
    expect(model.providers.map((p) => p.id)).toEqual(["p1", "p2"]);
  });
});
