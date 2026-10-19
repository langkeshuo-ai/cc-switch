import { useCallback, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Edit2, Globe, Layers, Plus, Search, Trash2 } from "lucide-react";
import { toast } from "@/lib/toast";
import type { AppId } from "@/lib/api";
import { providersApi, universalProvidersApi } from "@/lib/api";
import type {
  Provider,
  UniversalProvider,
  UniversalProvidersMap,
} from "@/types";
import type { ProviderEditorSave } from "@/lib/api/providers";
import { APP_DISPLAY_NAME } from "@/components/shell/AppGlyph";
import { AppPageHeader } from "@/components/shell/AppPageHeader";
import { ProviderIcon } from "@/components/ProviderIcon";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { EditProviderDialog } from "@/components/providers/EditProviderDialog";
import { Button } from "@/components/ui/button";
import { HelpTip } from "@/components/ui/help-tip";
import { HoverTip } from "@/components/ui/hover-tip";
import { Input } from "@/components/ui/input";
import { useProvidersQuery } from "@/lib/query";
import { useProxyStatus } from "@/hooks/useProxyStatus";
import { useProviderActions } from "@/hooks/useProviderActions";
import { deepClone } from "@/utils/deepClone";
import { extractErrorMessage } from "@/utils/errorUtils";
import { extractProviderBaseUrl } from "@/utils/providerConfigUtils";
import { AppRoutingPanel } from "./AppRoutingPanel";
import { UniversalProviderCard } from "./UniversalProviderCard";
import { UniversalProviderFormModal } from "./UniversalProviderFormModal";
import {
  buildUnifiedRows,
  currentAppsOf,
  filterUnifiedRows,
} from "./unifiedRows";

/**
 * 全部供应商（跨应用统一视图，magpie 式）：
 * 统一供应商一张卡、标注覆盖哪些应用、哪个应用正在使用；
 * 各应用自有的供应商跟在后面，编辑/删除走与应用页同一套流程。
 */
export function UnifiedProvidersPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { takeoverStatus } = useProxyStatus();

  // 统一供应商
  const universal = useQuery<UniversalProvidersMap>({
    queryKey: ["universalProviders"],
    queryFn: () => universalProvidersApi.getAll(),
  });

  // 各应用供应商列表：与应用页同一缓存，不重复请求
  const claude = useProvidersQuery("claude");
  const codex = useProvidersQuery("codex");
  const pi = useProvidersQuery("pi");
  const appData = useMemo(
    () => ({
      claude: {
        providers: claude.data?.providers ?? {},
        currentProviderId: claude.data?.currentProviderId ?? "",
      },
      codex: {
        providers: codex.data?.providers ?? {},
        currentProviderId: codex.data?.currentProviderId ?? "",
      },
      pi: {
        providers: pi.data?.providers ?? {},
        currentProviderId: pi.data?.currentProviderId ?? "",
      },
    }),
    [claude.data, codex.data, pi.data],
  );

  // 各应用动作（自有供应商的编辑/删除与应用页同流程）
  const actions = {
    claude: useProviderActions("claude", takeoverStatus?.["claude"] || false),
    codex: useProviderActions("codex", takeoverStatus?.["codex"] || false),
    pi: useProviderActions("pi"),
  };

  // 统一列表：统一供应商在前，各应用自有供应商在后；子供应商不单列
  const rows = useMemo(
    () => buildUnifiedRows(universal.data ?? {}, appData),
    [universal.data, appData],
  );

  const [query, setQuery] = useState("");
  const filtered = useMemo(() => filterUnifiedRows(rows, query), [rows, query]);

  const refresh = useCallback(async () => {
    await queryClient.invalidateQueries({ queryKey: ["universalProviders"] });
    await queryClient.invalidateQueries({ queryKey: ["providers"] });
    await queryClient.invalidateQueries({ queryKey: ["pi", "currentState"] });
    await providersApi.updateTrayMenu().catch(() => undefined);
  }, [queryClient]);

  // ── 统一供应商动作 ──────────────────────────────────────────────

  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<UniversalProvider | null>(null);

  const openAdd = useCallback(() => {
    setEditing(null);
    setFormOpen(true);
  }, []);

  const openEdit = useCallback((p: UniversalProvider) => {
    setEditing(p);
    setFormOpen(true);
  }, []);

  const handleUniversalSave = useCallback(
    async (provider: UniversalProvider) => {
      try {
        // 后端保存即自动同步到各启用应用
        await universalProvidersApi.upsert(provider);
        toast.success(
          t(
            editing
              ? "universalProvider.updated"
              : "universalProvider.addedAndSynced",
            {
              defaultValue: editing
                ? "统一供应商已更新"
                : "统一供应商已添加并同步",
            },
          ),
        );
        setFormOpen(false);
        setEditing(null);
        await refresh();
      } catch (error) {
        console.error("Failed to save universal provider:", error);
        toast.error(
          extractErrorMessage(error) ||
            t("universalProvider.saveError", {
              defaultValue: "保存统一供应商失败",
            }),
        );
      }
    },
    [editing, refresh, t],
  );

  const handleUniversalDuplicate = useCallback(
    async (provider: UniversalProvider) => {
      const duplicated: UniversalProvider = {
        ...deepClone(provider),
        id: crypto.randomUUID(),
        name: `${provider.name} copy`,
        createdAt: Date.now(),
      };
      try {
        await universalProvidersApi.upsert(duplicated);
        toast.success(
          t("universalProvider.duplicatedAndSynced", {
            defaultValue: "统一供应商已复制并同步",
          }),
        );
        await refresh();
      } catch (error) {
        console.error("Failed to duplicate universal provider:", error);
        toast.error(
          extractErrorMessage(error) ||
            t("universalProvider.duplicateError", {
              defaultValue: "复制统一供应商失败",
            }),
        );
      }
    },
    [refresh, t],
  );

  const handleUniversalSync = useCallback(
    async (id: string) => {
      try {
        await universalProvidersApi.sync(id);
        toast.success(
          t("universalProvider.synced", { defaultValue: "已同步到所有应用" }),
        );
        await refresh();
      } catch (error) {
        console.error("Failed to sync universal provider:", error);
        toast.error(
          extractErrorMessage(error) ||
            t("universalProvider.syncError", {
              defaultValue: "同步统一供应商失败",
            }),
        );
      }
    },
    [refresh, t],
  );

  const [deleteConfirm, setDeleteConfirm] = useState<{
    open: boolean;
    id: string;
    name: string;
  }>({ open: false, id: "", name: "" });

  const handleDeleteConfirm = useCallback(async () => {
    if (!deleteConfirm.id) return;
    try {
      // 后端级联删除各应用子供应商；子供应商正在使用时会拒绝并说明原因
      await universalProvidersApi.delete(deleteConfirm.id);
      toast.success(
        t("universalProvider.deleted", { defaultValue: "统一供应商已删除" }),
      );
      await refresh();
    } catch (error) {
      console.error("Failed to delete universal provider:", error);
      toast.error(
        extractErrorMessage(error) ||
          t("universalProvider.deleteError", {
            defaultValue: "删除统一供应商失败",
          }),
      );
    } finally {
      setDeleteConfirm({ open: false, id: "", name: "" });
    }
  }, [deleteConfirm.id, refresh, t]);

  // ── 各应用自有供应商动作 ────────────────────────────────────────

  const [perAppEdit, setPerAppEdit] = useState<{
    app: AppId;
    provider: Provider;
  } | null>(null);
  const [perAppDelete, setPerAppDelete] = useState<{
    app: AppId;
    provider: Provider;
  } | null>(null);

  const handlePerAppEditSubmit = useCallback(
    async ({
      provider,
      originalId,
      editorSave,
    }: {
      provider: Provider;
      originalId?: string;
      editorSave?: ProviderEditorSave;
    }) => {
      if (!perAppEdit) return;
      await actions[perAppEdit.app].updateProvider(
        provider,
        originalId,
        editorSave,
      );
      setPerAppEdit(null);
    },
    [perAppEdit, actions],
  );

  const handlePerAppDeleteConfirm = useCallback(async () => {
    if (!perAppDelete) return;
    const { app, provider } = perAppDelete;
    setPerAppDelete(null);
    try {
      await actions[app].deleteProvider(provider.id);
    } catch {
      // 删除失败时 mutation 的 onError 已弹错误提示
    }
  }, [perAppDelete, actions]);

  const loading =
    universal.isLoading || claude.isLoading || codex.isLoading || pi.isLoading;

  return (
    <>
      <AppPageHeader
        icon={<Layers className="h-5 w-5" strokeWidth={1.5} />}
        title={t("nav.allProviders", { defaultValue: "全部供应商" })}
        titleExtra={
          <HelpTip
            title={t("allProviders.helpTitle", { defaultValue: "全部供应商" })}
          >
            {t("allProviders.helpBody", {
              defaultValue:
                "在一个列表里查看和管理所有应用的供应商。统一供应商添加或修改一次，即在所有启用的应用生效。",
            })}
          </HelpTip>
        }
        actions={
          <>
            <div className="relative">
              <Search className="pointer-events-none absolute start-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-fg-3" />
              <Input
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={t("allProviders.searchPlaceholder", {
                  defaultValue: "搜索名称或 API 地址",
                })}
                className="w-56 ps-8"
              />
            </div>
            <Button variant="solid" size="regular" onClick={openAdd}>
              <Plus className="h-4 w-4" />
              {t("allProviders.add", { defaultValue: "添加供应商" })}
            </Button>
          </>
        }
      />

      <div
        id="main-content"
        className="min-h-0 flex-1 scroll-stable overflow-y-auto overflow-x-hidden px-6 pb-12 pt-4"
      >
        <div className="space-y-4">
          <AppRoutingPanel
            appData={appData}
            onSwitch={(app, provider) => actions[app].switchProvider(provider)}
          />
          {loading ? (
            <div className="flex items-center justify-center py-12">
              <div className="h-6 w-6 animate-spin rounded-full border-2 border-primary border-t-transparent" />
            </div>
          ) : filtered.length === 0 ? (
            <div className="flex flex-col items-center justify-center rounded-xl border border-dashed py-12 text-center">
              <Layers className="mb-3 h-10 w-10 text-fg-3" />
              <p className="text-sm text-fg-2">
                {query
                  ? t("allProviders.noMatch", {
                      defaultValue: "没有匹配的供应商",
                    })
                  : t("allProviders.empty", { defaultValue: "还没有供应商" })}
              </p>
              <p className="mt-1 text-xs text-fg-3">
                {t("allProviders.emptyHint", {
                  defaultValue:
                    "点右上角「添加供应商」创建。统一供应商只需添加一次，即在所有启用的应用生效",
                })}
              </p>
            </div>
          ) : (
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {filtered.map((row) =>
                row.kind === "universal" ? (
                  <UniversalProviderCard
                    key={`u-${row.provider.id}`}
                    provider={row.provider}
                    currentApps={currentAppsOf(row.provider, appData)}
                    onEdit={openEdit}
                    onDelete={(id) =>
                      setDeleteConfirm({
                        open: true,
                        id,
                        name: row.provider.name,
                      })
                    }
                    onSync={handleUniversalSync}
                    onDuplicate={handleUniversalDuplicate}
                  />
                ) : (
                  <PerAppProviderCard
                    key={`${row.app}-${row.provider.id}`}
                    app={row.app}
                    provider={row.provider}
                    isCurrent={
                      row.provider.id === appData[row.app].currentProviderId
                    }
                    onEdit={() =>
                      setPerAppEdit({ app: row.app, provider: row.provider })
                    }
                    onDelete={() =>
                      setPerAppDelete({ app: row.app, provider: row.provider })
                    }
                  />
                ),
              )}
            </div>
          )}
        </div>
      </div>

      <UniversalProviderFormModal
        isOpen={formOpen}
        onClose={() => {
          setFormOpen(false);
          setEditing(null);
        }}
        onSave={handleUniversalSave}
        editingProvider={editing}
      />

      {perAppEdit && (
        <EditProviderDialog
          key={`${perAppEdit.app}-${perAppEdit.provider.id}`}
          open={Boolean(perAppEdit)}
          provider={perAppEdit.provider}
          onOpenChange={(open) => {
            if (!open) setPerAppEdit(null);
          }}
          onSubmit={handlePerAppEditSubmit}
          appId={perAppEdit.app}
          isProxyTakeover={
            perAppEdit.app === "pi"
              ? false
              : takeoverStatus?.[perAppEdit.app] || false
          }
          isCurrent={
            perAppEdit.provider.id === appData[perAppEdit.app].currentProviderId
          }
        />
      )}

      <ConfirmDialog
        isOpen={deleteConfirm.open}
        title={t("universalProvider.deleteConfirmTitle", {
          defaultValue: "删除统一供应商",
        })}
        message={t("universalProvider.deleteConfirmDescription", {
          defaultValue: `确定要删除 "${deleteConfirm.name}" 吗？这将同时删除它在各应用中生成的供应商配置。`,
          name: deleteConfirm.name,
        })}
        confirmText={t("common.delete", { defaultValue: "删除" })}
        onConfirm={() => void handleDeleteConfirm()}
        onCancel={() => setDeleteConfirm({ open: false, id: "", name: "" })}
      />

      <ConfirmDialog
        isOpen={Boolean(perAppDelete)}
        title={t("confirm.deleteProvider", { defaultValue: "删除供应商" })}
        message={
          perAppDelete
            ? t("confirm.deleteProviderMessage", {
                defaultValue: `确定要删除供应商 "${perAppDelete.provider.name}" 吗？`,
                name: perAppDelete.provider.name,
              })
            : ""
        }
        confirmText={t("common.delete", { defaultValue: "删除" })}
        onConfirm={() => void handlePerAppDeleteConfirm()}
        onCancel={() => setPerAppDelete(null)}
      />
    </>
  );
}

/** 单个应用自有供应商的卡片（统一视图里不带切换/用量等应用页动作） */
function PerAppProviderCard({
  app,
  provider,
  isCurrent,
  onEdit,
  onDelete,
}: {
  app: AppId;
  provider: Provider;
  isCurrent: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const baseUrl = extractProviderBaseUrl(provider.settingsConfig);
  return (
    <div className="group relative rounded-xl border border-border/50 bg-surface p-4 transition-all hover:border-border hover:shadow-md">
      <div className="flex items-start justify-between">
        <div className="flex min-w-0 items-center gap-3">
          <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-subtle">
            <ProviderIcon icon={provider.icon} name={provider.name} size={24} />
          </div>
          <div className="min-w-0">
            <h3 className="truncate font-semibold text-fg-1">
              {provider.name}
            </h3>
            <div className="mt-1 flex flex-wrap items-center gap-1.5">
              <span className="inline-flex items-center rounded-full bg-subtle px-2 py-0.5 text-xs font-medium text-fg-2">
                {APP_DISPLAY_NAME[app]}
              </span>
              {isCurrent && (
                <span className="inline-flex items-center rounded-full bg-primary/10 px-2 py-0.5 text-xs font-medium text-primary">
                  {t("allProviders.current", { defaultValue: "使用中" })}
                </span>
              )}
            </div>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-1 opacity-0 transition-opacity group-focus-within:opacity-100 group-hover:opacity-100">
          <HoverTip content={t("common.edit", { defaultValue: "编辑" })}>
            <Button
              variant="ghost"
              size="icon"
              className="h-8 w-8"
              onClick={onEdit}
              aria-label={t("common.edit", { defaultValue: "编辑" })}
            >
              <Edit2 className="h-4 w-4" />
            </Button>
          </HoverTip>
          <HoverTip content={t("common.delete", { defaultValue: "删除" })}>
            <Button
              variant="ghost"
              size="icon"
              className="h-8 w-8 text-destructive hover:text-destructive"
              onClick={onDelete}
              aria-label={t("common.delete", { defaultValue: "删除" })}
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </HoverTip>
        </div>
      </div>

      {baseUrl && (
        <div className="mt-4 flex items-center gap-2 text-sm">
          <Globe className="h-3.5 w-3.5 shrink-0 text-fg-2" />
          <span className="truncate text-fg-2">{baseUrl}</span>
        </div>
      )}
    </div>
  );
}
