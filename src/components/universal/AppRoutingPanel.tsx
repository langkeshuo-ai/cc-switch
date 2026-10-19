import { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "@/lib/toast";
import type { AppId } from "@/lib/api";
import type { Provider } from "@/types";
import { APP_ICON_MAP, type ProxyAppId } from "@/config/appConfig";
import { APP_DISPLAY_NAME } from "@/components/shell/AppGlyph";
import { MODE_TONE } from "@/components/providers/mode/ModeTabs";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Button } from "@/components/ui/button";
import { HelpTip } from "@/components/ui/help-tip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useModeActions } from "@/hooks/useModeActions";
import { useProxyStatus } from "@/hooks/useProxyStatus";
import { useAppMode } from "@/lib/query/proxy";
import { usePiCurrentState } from "@/lib/query/pi";
import { cn } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";
import { buildAppRoutingModel, entryRouteTarget } from "./unifiedRouting";
import type { AppProviderData, AppProviderDataMap } from "./unifiedRows";

/**
 * 应用路由（magpie 式统一开关）：
 * 每个应用一张卡——路由开关（开 = 路由/聚合，关 = 直连）、生效供应商下拉、
 * 路由/聚合子模式；pi 只控制默认供应商。
 * 动作全部复用应用页同一套流程（switchProvider / 模式进出），不新增后端。
 */
interface AppRoutingPanelProps {
  appData: AppProviderDataMap;
  onSwitch: (app: AppId, provider: Provider) => unknown;
}

export function AppRoutingPanel({ appData, onSwitch }: AppRoutingPanelProps) {
  const { t } = useTranslation();
  return (
    <section className="space-y-2">
      <div className="flex items-center gap-2">
        <h2 className="text-sm font-semibold text-fg-1">
          {t("allProviders.routingTitle", { defaultValue: "应用路由" })}
        </h2>
        <HelpTip
          title={t("allProviders.routingTitle", { defaultValue: "应用路由" })}
        >
          {t("allProviders.routingHelp", {
            defaultValue:
              "每个应用的路由开关、模式与生效供应商在此统一管理。打开路由后，请求先经过本机 CC Switch 转发。",
          })}
        </HelpTip>
      </div>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        <ProxyRoutingRow
          app="claude"
          data={appData.claude}
          onSwitch={onSwitch}
        />
        <ProxyRoutingRow app="codex" data={appData.codex} onSwitch={onSwitch} />
        <PiRoutingRow data={appData.pi} onSwitch={onSwitch} />
      </div>
    </section>
  );
}

/** claude / codex：路由开关 + 生效供应商 + 路由/聚合子模式 */
function ProxyRoutingRow({
  app,
  data,
  onSwitch,
}: {
  app: ProxyAppId;
  data: AppProviderData;
  onSwitch: (app: AppId, provider: Provider) => unknown;
}) {
  const { t } = useTranslation();
  const { data: modeView } = useAppMode(app);
  const modeActions = useModeActions(app);
  const { isRunning, isStarting, startProxyServer } = useProxyStatus();
  const [modeConfirm, setModeConfirm] = useState<"route" | "stack" | null>(
    null,
  );

  const model = buildAppRoutingModel(app, data, modeView, null);
  const entryTarget = entryRouteTarget(modeView, data.currentProviderId);
  const entryTargetName = entryTarget
    ? (data.providers[entryTarget]?.name ?? null)
    : null;
  const appName = APP_DISPLAY_NAME[app];

  // 开路由：进路由模式要先过确认框（写明后果）
  const handleToggle = (on: boolean) => {
    if (on) {
      setModeConfirm("route");
      return;
    }
    if (!model.mode) return;
    const directId = modeView?.directProviderId ?? null;
    const providerName =
      (directId ? data.providers[directId]?.name : undefined) ??
      model.effectiveProviderName ??
      "";
    void modeActions.exitToDirect({
      previous: {
        mode: model.mode,
        routeProviderId: modeView?.routeProviderId ?? null,
      },
      directProviderId: directId,
      providerName,
    });
  };

  const handleModeConfirm = async () => {
    const mode = modeConfirm;
    setModeConfirm(null);
    if (!mode) return;
    try {
      // 路由模式需要本地服务在跑；没跑就先启动，启动失败则不进模式
      if (!isRunning) await startProxyServer();
      await modeActions.enter(mode, entryTarget, entryTargetName ?? undefined);
    } catch (error) {
      toast.error(
        t("mode.toast.failed", {
          detail:
            extractErrorMessage(error) ||
            t("common.unknown", { defaultValue: "未知错误" }),
        }),
      );
    }
  };

  const confirmMessage = modeConfirm
    ? [
        t(
          modeConfirm === "route"
            ? "mode.dialog.routeLead"
            : "mode.dialog.stackLead",
        ),
        entryTargetName
          ? `${t(
              modeConfirm === "route"
                ? "mode.dialog.routeTo"
                : "mode.dialog.stackDefault",
            )}：${entryTargetName}`
          : "",
        modeConfirm === "route"
          ? t("mode.dialog.routeNoteKeepRunning")
          : t("mode.dialog.stackNoteNoFailover"),
      ]
        .filter(Boolean)
        .join(" ")
    : "";

  return (
    <div className="flex flex-col rounded-xl border border-border/50 bg-surface p-4">
      <div className="flex items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <div
            className={cn(
              "flex h-9 w-9 shrink-0 items-center justify-center rounded-lg",
              APP_ICON_MAP[app].badgeClass,
            )}
          >
            {APP_ICON_MAP[app].icon}
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-1.5">
              <h3 className="truncate text-sm font-semibold text-fg-1">
                {appName}
              </h3>
              {model.mode && (
                <span
                  className={cn(
                    "inline-flex shrink-0 items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium",
                    MODE_TONE[model.mode].soft,
                    MODE_TONE[model.mode].text,
                  )}
                >
                  <span
                    className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      MODE_TONE[model.mode].dot,
                    )}
                  />
                  {t(`mode.names.${model.mode}`)}
                </span>
              )}
            </div>
            <p className="mt-0.5 truncate text-xs text-fg-2">
              {model.effectiveProviderName ??
                t("allProviders.routingNone", {
                  defaultValue: "未选择供应商",
                })}
            </p>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <span className="text-xs text-fg-2">
            {t("allProviders.routingSwitch", { defaultValue: "路由" })}
          </span>
          <Switch
            checked={model.routingOn}
            onCheckedChange={(on) => handleToggle(on)}
            disabled={modeActions.pending}
            aria-label={`${appName} ${t("allProviders.routingSwitch", {
              defaultValue: "路由",
            })}`}
          />
        </div>
      </div>

      <div className="mt-3 flex-1" />

      <Select
        value={model.effectiveProviderId ?? ""}
        onValueChange={(id) => {
          const provider = data.providers[id];
          if (provider) void onSwitch(app, provider);
        }}
        disabled={model.providers.length === 0 || modeActions.pending}
      >
        <SelectTrigger className="w-full">
          <SelectValue
            placeholder={t("allProviders.routingSelect", {
              defaultValue: "选择供应商",
            })}
          />
        </SelectTrigger>
        <SelectContent>
          {model.providers.map((p) => (
            <SelectItem key={p.id} value={p.id}>
              {p.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>

      {model.routingOn && model.stackSupported && (
        <div className="mt-2 flex rounded-lg bg-subtle p-0.5">
          {(["route", "stack"] as const).map((m) => (
            <button
              key={m}
              type="button"
              onClick={() => {
                if (model.mode !== m) setModeConfirm(m);
              }}
              className={cn(
                "flex-1 rounded-md px-2 py-1 text-xs transition-colors",
                model.mode === m
                  ? "bg-surface font-medium text-fg-1 shadow-sm"
                  : "text-fg-2 hover:text-fg-1",
              )}
            >
              {t(`mode.names.${m}`)}
            </button>
          ))}
        </div>
      )}

      {model.routingOn && !isRunning && (
        <div className="mt-2 flex items-center justify-between gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2">
          <span className="text-xs text-fg-2">
            {t("mode.notice.serviceDown", {
              app: appName,
              mode: t(`mode.names.${model.mode ?? "route"}`),
            })}
          </span>
          <Button
            variant="outline"
            size="sm"
            className="shrink-0"
            onClick={() => void startProxyServer()}
            disabled={isStarting}
          >
            {t("mode.notice.startService")}
          </Button>
        </div>
      )}

      <ConfirmDialog
        isOpen={modeConfirm !== null}
        title={t(
          modeConfirm === "stack"
            ? "mode.dialog.stackTitle"
            : "mode.dialog.routeTitle",
          { app: appName },
        )}
        message={confirmMessage}
        confirmText={t(
          modeConfirm === "stack"
            ? "mode.dialog.confirmStack"
            : "mode.dialog.confirmRoute",
        )}
        onConfirm={() => void handleModeConfirm()}
        onCancel={() => setModeConfirm(null)}
      />
    </div>
  );
}

/** pi：累加式，无路由概念，只控制默认供应商 */
function PiRoutingRow({
  data,
  onSwitch,
}: {
  data: AppProviderData;
  onSwitch: (app: AppId, provider: Provider) => unknown;
}) {
  const { t } = useTranslation();
  const { data: piState } = usePiCurrentState();
  const providers = Object.values(data.providers);
  const defaultId = piState?.defaultProviderId ?? "";
  const enabledCount = piState?.enabledProviderIds.length ?? 0;

  return (
    <div className="flex flex-col rounded-xl border border-border/50 bg-surface p-4">
      <div className="flex items-center gap-2.5">
        <div
          className={cn(
            "flex h-9 w-9 shrink-0 items-center justify-center rounded-lg",
            APP_ICON_MAP.pi.badgeClass,
          )}
        >
          {APP_ICON_MAP.pi.icon}
        </div>
        <div className="min-w-0">
          <h3 className="truncate text-sm font-semibold text-fg-1">
            {APP_DISPLAY_NAME.pi}
          </h3>
          <p className="mt-0.5 text-xs text-fg-2">
            {t("allProviders.routingEnabledCount", {
              count: enabledCount,
              defaultValue: "{{count}} 家已启用",
            })}
          </p>
        </div>
      </div>

      <div className="mt-3 flex-1" />

      <label className="mb-1 text-xs text-fg-2">
        {t("allProviders.routingPiDefault", { defaultValue: "默认供应商" })}
      </label>
      <Select
        value={defaultId}
        onValueChange={(id) => {
          const provider = data.providers[id];
          if (provider) void onSwitch("pi", provider);
        }}
        disabled={providers.length === 0}
      >
        <SelectTrigger className="w-full">
          <SelectValue
            placeholder={t("allProviders.routingSelect", {
              defaultValue: "选择供应商",
            })}
          />
        </SelectTrigger>
        <SelectContent>
          {providers.map((p) => (
            <SelectItem key={p.id} value={p.id}>
              {p.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}
