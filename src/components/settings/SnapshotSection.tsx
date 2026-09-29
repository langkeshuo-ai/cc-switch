import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Camera, Loader2, RotateCcw, Save, Trash2 } from "lucide-react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { snapshotsApi, type AppSnapshotMeta } from "@/lib/api";

const SNAPSHOT_APPS = ["claude", "codex", "pi"] as const;

/** 把后端原因码（"code" 或 "code: detail"）转为可展示文本 */
function formatSkipReason(reason: string, t: (key: string) => string): string {
  const sep = reason.indexOf(": ");
  const code = sep === -1 ? reason : reason.slice(0, sep);
  const detail = sep === -1 ? "" : reason.slice(sep + 2);
  const key = `settings.snapshot.skipReason.${code}`;
  const label = t(key);
  // 未命中词条时 i18next 会原样返回 key，此时展示原始 reason
  if (label === key) return reason;
  return detail ? `${label}: ${detail}` : label;
}

export function SnapshotSection() {
  const { t } = useTranslation();
  const [snapshots, setSnapshots] = useState<AppSnapshotMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<{
    kind: "apply" | "delete" | "overwrite";
    target: string;
  } | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await snapshotsApi.list();
      setSnapshots(list);
    } catch (error) {
      console.error("[SnapshotSection] Failed to load snapshots", error);
      toast.error(t("settings.snapshot.loadFailed"));
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const doSave = useCallback(
    async (target: string) => {
      setBusy(true);
      try {
        await snapshotsApi.save(target);
        toast.success(t("settings.snapshot.saved"));
        setName("");
        await refresh();
      } catch (error) {
        console.error("[SnapshotSection] Failed to save snapshot", error);
        toast.error(t("settings.snapshot.saveFailed"));
      } finally {
        setBusy(false);
      }
    },
    [refresh, t],
  );

  const handleSaveClick = useCallback(() => {
    const target = name.trim();
    if (!target) {
      toast.error(t("settings.snapshot.nameRequired"));
      return;
    }
    if (snapshots.some((s) => s.name === target)) {
      setConfirm({ kind: "overwrite", target });
      return;
    }
    doSave(target);
  }, [doSave, name, snapshots, t]);

  const doApply = useCallback(
    async (target: string) => {
      setBusy(true);
      try {
        const result = await snapshotsApi.apply(target);
        if (result.skipped.length === 0) {
          toast.success(t("settings.snapshot.applied"));
        } else {
          const detail = result.skipped
            .map((s) => `${s.app}: ${formatSkipReason(s.reason, t)}`)
            .join("\n");
          toast.warning(t("settings.snapshot.applyPartial"), {
            description: detail,
          });
        }
        await refresh();
      } catch (error) {
        console.error("[SnapshotSection] Failed to apply snapshot", error);
        toast.error(t("settings.snapshot.applyFailed"));
      } finally {
        setBusy(false);
      }
    },
    [refresh, t],
  );

  const doDelete = useCallback(
    async (target: string) => {
      setBusy(true);
      try {
        await snapshotsApi.remove(target);
        toast.success(t("settings.snapshot.deleted"));
        await refresh();
      } catch (error) {
        console.error("[SnapshotSection] Failed to delete snapshot", error);
        toast.error(t("settings.snapshot.deleteFailed"));
      } finally {
        setBusy(false);
      }
    },
    [refresh, t],
  );

  const handleConfirm = useCallback(
    (checked: boolean) => {
      if (!confirm) return;
      const { kind, target } = confirm;
      setConfirm(null);
      if (kind === "apply") void doApply(target);
      else if (kind === "delete") void doDelete(target);
      else if (kind === "overwrite") void doSave(target);
      void checked;
    },
    [confirm, doApply, doDelete, doSave],
  );

  return (
    <section className="space-y-4">
      <header className="space-y-1">
        <h3 className="text-sm font-medium flex items-center gap-2">
          <Camera className="h-4 w-4 text-primary" />
          {t("settings.snapshot.title")}
        </h3>
        <p className="text-xs text-muted-foreground">
          {t("settings.snapshot.description")}
        </p>
      </header>

      <div className="flex gap-2">
        <Input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={t("settings.snapshot.savePlaceholder")}
          disabled={busy}
          onKeyDown={(e) => {
            if (e.key === "Enter") handleSaveClick();
          }}
        />
        <Button variant="outline" onClick={handleSaveClick} disabled={busy}>
          {busy ? (
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
          ) : (
            <Save className="mr-2 h-4 w-4" />
          )}
          {t("settings.snapshot.save")}
        </Button>
      </div>

      {loading ? (
        <div className="flex justify-center py-4">
          <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
        </div>
      ) : snapshots.length === 0 ? (
        <p className="text-xs text-muted-foreground py-2">
          {t("settings.snapshot.empty")}
        </p>
      ) : (
        <ul className="space-y-2">
          {snapshots.map((snapshot) => (
            <li
              key={snapshot.name}
              className="rounded-lg border border-border/60 p-3 space-y-2"
            >
              <div className="flex items-center justify-between gap-2">
                <div className="min-w-0">
                  <p className="text-sm font-medium truncate">
                    {snapshot.name}
                  </p>
                  <p className="text-xs text-muted-foreground">
                    {new Date(snapshot.createdAt).toLocaleString()}
                  </p>
                </div>
                <div className="flex items-center gap-2 flex-shrink-0">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() =>
                      setConfirm({ kind: "apply", target: snapshot.name })
                    }
                    disabled={busy}
                  >
                    <RotateCcw className="mr-1.5 h-3.5 w-3.5" />
                    {t("settings.snapshot.apply")}
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="text-destructive hover:text-destructive"
                    onClick={() =>
                      setConfirm({ kind: "delete", target: snapshot.name })
                    }
                    disabled={busy}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                  </Button>
                </div>
              </div>
              <div className="flex flex-wrap gap-x-4 gap-y-1">
                {SNAPSHOT_APPS.map((app) => {
                  const entry = snapshot.apps[app];
                  if (!entry) return null;
                  return (
                    <span
                      key={app}
                      className="inline-flex items-center gap-1.5 text-xs text-muted-foreground"
                    >
                      <span className="capitalize">{app}</span>
                      <span className="text-foreground">
                        {entry.providerName ??
                          t("settings.snapshot.deletedProvider")}
                      </span>
                      <Badge
                        variant={entry.takeover ? "default" : "outline"}
                        className="px-1.5 py-0 text-[10px]"
                      >
                        {entry.takeover
                          ? t("settings.snapshot.takeoverOn")
                          : t("settings.snapshot.takeoverOff")}
                      </Badge>
                    </span>
                  );
                })}
              </div>
            </li>
          ))}
        </ul>
      )}

      <ConfirmDialog
        isOpen={confirm !== null}
        title={
          confirm?.kind === "apply"
            ? t("settings.snapshot.applyTitle")
            : confirm?.kind === "overwrite"
              ? t("settings.snapshot.overwriteTitle")
              : t("settings.snapshot.deleteTitle")
        }
        message={
          confirm?.kind === "apply"
            ? t("settings.snapshot.applyMessage")
            : confirm?.kind === "overwrite"
              ? t("settings.snapshot.overwriteMessage", {
                  name: confirm?.target ?? "",
                })
              : t("settings.snapshot.deleteMessage", {
                  name: confirm?.target ?? "",
                })
        }
        variant={confirm?.kind === "apply" ? "info" : "destructive"}
        confirmText={t("common.confirm")}
        cancelText={t("common.cancel")}
        pending={busy}
        onConfirm={handleConfirm}
        onCancel={() => setConfirm(null)}
      />
    </section>
  );
}
