import React from "react";
import type { AppId } from "@/lib/api/types";
import type { VisibleApps } from "@/types";
import { ClaudeIcon, CodexIcon } from "@/components/BrandIcons";
import { ProviderIcon } from "@/components/ProviderIcon";

export interface AppConfig {
  label: string;
  icon: React.ReactNode;
  activeClass: string;
  badgeClass: string;
}

export const APP_IDS: AppId[] = ["claude", "codex", "pi"];

export const DEFAULT_VISIBLE_APPS: VisibleApps = {
  claude: true,
  codex: true,
  pi: true,
};

/** App IDs shown in Skills panels. */
export const SKILLS_APP_IDS: AppId[] = ["claude", "codex", "pi"];

export type ProxyAppId = Extract<AppId, "claude" | "codex" | "pi">;

/** Apps with a complete local gateway + failover data plane. */
export const PROXY_APP_IDS: ProxyAppId[] = ["claude", "codex", "pi"];

export function isProxyAppId(appId: string): appId is ProxyAppId {
  return (PROXY_APP_IDS as string[]).includes(appId);
}

export type AdditiveAppId = Extract<AppId, "pi">;

export const ADDITIVE_APP_IDS: AdditiveAppId[] = ["pi"];

export function isAdditiveAppId(appId: string): appId is AdditiveAppId {
  return (ADDITIVE_APP_IDS as string[]).includes(appId);
}

/** Pi has no native MCP registry; do not manufacture a disabled mirror. */
export type McpAppId = Exclude<AppId, "pi">;
export const MCP_APP_IDS: McpAppId[] = ["claude", "codex"];

export function isMcpAppId(appId: string): appId is McpAppId {
  return (MCP_APP_IDS as string[]).includes(appId);
}

export const APP_ICON_MAP: Record<AppId, AppConfig> = {
  claude: {
    label: "Claude",
    icon: <ClaudeIcon size={14} />,
    activeClass:
      "bg-orange-500/10 ring-1 ring-orange-500/20 hover:bg-orange-500/20 text-orange-600 dark:text-orange-400",
    badgeClass:
      "bg-orange-500/10 text-orange-700 dark:text-orange-300 hover:bg-orange-500/20 border-0 gap-1.5",
  },
  codex: {
    label: "Codex",
    icon: <CodexIcon size={14} />,
    activeClass:
      "bg-green-500/10 ring-1 ring-green-500/20 hover:bg-green-500/20 text-green-600 dark:text-green-400",
    badgeClass:
      "bg-green-500/10 text-green-700 dark:text-green-300 hover:bg-green-500/20 border-0 gap-1.5",
  },
  pi: {
    label: "Pi",
    icon: <ProviderIcon icon="pi" name="Pi" size={14} showFallback={false} />,
    activeClass:
      "bg-fuchsia-500/10 ring-1 ring-fuchsia-500/20 hover:bg-fuchsia-500/20 text-fuchsia-600 dark:text-fuchsia-400",
    badgeClass:
      "bg-fuchsia-500/10 text-fuchsia-700 dark:text-fuchsia-300 hover:bg-fuchsia-500/20 border-0 gap-1.5",
  },
};

export function getAppLabel(appId: string): string {
  return APP_ICON_MAP[appId as AppId]?.label ?? appId;
}
