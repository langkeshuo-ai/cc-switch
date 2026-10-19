import type { AppId } from "@/lib/api";
import type { Provider } from "@/types";
import type { AppMode, AppModeView } from "@/types/proxy";
import { isProxyAppId, isStackAppId } from "@/config/appConfig";
import type { AppProviderData } from "./unifiedRows";

/** 路由是否开启：处于路由或聚合模式即算开（pi 无路由概念，恒为关）。 */
export function isRoutingOn(mode: AppMode | null | undefined): boolean {
  return mode === "route" || mode === "stack";
}

/**
 * 从直连切入路由时默认路由到哪家：优先上次路由的那家，其次当前直连的。
 * 两者都没有时返回 null（后端会沿用上次的路由）。
 */
export function entryRouteTarget(
  modeView: AppModeView | undefined,
  currentProviderId: string,
): string | null {
  return modeView?.routeProviderId || currentProviderId || null;
}

export interface AppRoutingModel {
  app: AppId;
  /** 有路由的切换式应用（claude/codex）；pi 为 false */
  isProxy: boolean;
  /** 生效的模式；pi 为 null */
  mode: AppMode | null;
  routingOn: boolean;
  stackSupported: boolean;
  /**
   * 实际生效的供应商：路由/聚合模式下后端 current 已返回路由到的那家，
   * 直连模式是直连那家，pi 是默认那家。
   */
  effectiveProviderId: string | null;
  effectiveProviderName: string | null;
  providers: Provider[];
}

/** 每应用路由模型（纯函数，供单元测试）。 */
export function buildAppRoutingModel(
  app: AppId,
  data: AppProviderData,
  modeView: AppModeView | undefined,
  piDefaultProviderId: string | null,
): AppRoutingModel {
  const isProxy = isProxyAppId(app);
  const mode: AppMode | null = isProxy ? (modeView?.mode ?? "direct") : null;
  const effectiveProviderId = isProxy
    ? data.currentProviderId || null
    : piDefaultProviderId;
  const providers = Object.values(data.providers);
  return {
    app,
    isProxy,
    mode,
    routingOn: isRoutingOn(mode),
    stackSupported: isStackAppId(app),
    effectiveProviderId,
    effectiveProviderName: effectiveProviderId
      ? (providers.find((p) => p.id === effectiveProviderId)?.name ?? null)
      : null,
    providers,
  };
}
