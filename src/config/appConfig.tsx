import React from "react";
import type { AppId } from "@/lib/api/types";
import type { VisibleApps } from "@/types";
import {
  ClaudeIcon,
  CodexIcon,
} from "@/components/BrandIcons";
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

export type ProxyAppId = Extract<AppId, "claude" | "codex">;

/** Apps with a complete local gateway + failover data plane. */
export const PROXY_APP_IDS: ProxyAppId[] = ["claude", "codex"];

export function isProxyAppId(appId: string): appId is ProxyAppId {
  return (PROXY_APP_IDS as string[]).includes(appId);
}

/** 支持 Stack 模式的应用（后端 `mode::stack::supports_stack` 的镜像）。 */
export type StackAppId = Extract<ProxyAppId, "claude" | "codex">;

export const STACK_APP_IDS: StackAppId[] = ["claude", "codex"];

export function isStackAppId(appId: string): appId is StackAppId {
  return (STACK_APP_IDS as string[]).includes(appId);
}

export type AdditiveAppId = Extract<AppId, "pi">;

export const ADDITIVE_APP_IDS: AdditiveAppId[] = ["pi"];

export function isAdditiveAppId(appId: string): appId is AdditiveAppId {
  return (ADDITIVE_APP_IDS as string[]).includes(appId);
}

/**
 * 切换只替换关键字段的应用：供应商编辑器显示「切到这个供应商之后配置文件的样子」，由后端
 * `ProviderService::editor_view` 投影。
 */
export const EDITOR_VIEW_APP_IDS: AppId[] = ["claude", "codex"];

export function usesEditorView(appId: AppId): boolean {
  return EDITOR_VIEW_APP_IDS.includes(appId);
}

/** 三个保留应用均由 CC Switch 管理 MCP；Pi 1.0 起内置 MCP（`~/.pi/agent/mcp.json`，上游 4.0） */
export type McpAppId = AppId;
export const MCP_APP_IDS: McpAppId[] = ["claude", "codex", "pi"];

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
