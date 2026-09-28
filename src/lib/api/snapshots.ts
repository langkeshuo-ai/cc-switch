import { invoke } from "@tauri-apps/api/core";

/** 支持快照的应用（与后端 AppType::all() 严格对应） */
export type SnapshotAppId = "claude" | "codex" | "pi";

/** 列表展示用的单应用槽位（providerName 为 null 表示供应商已删除） */
export interface SnapshotAppMeta {
  providerId: string | null;
  providerName: string | null;
  takeover: boolean;
}

/** 快照元信息（与后端 SnapshotMeta 严格对应） */
export interface AppSnapshotMeta {
  name: string;
  createdAt: number;
  apps: Record<SnapshotAppId, SnapshotAppMeta>;
}

/** 应用快照时被跳过的应用（reason 为后端稳定原因码，可带 ": detail" 后缀） */
export interface SkippedApp {
  app: string;
  reason: string;
}

/** 应用快照的结果（与后端 SnapshotApplyResult 严格对应） */
export interface SnapshotApplyResult {
  applied: string[];
  skipped: SkippedApp[];
}

export const snapshotsApi = {
  /** 列出所有快照 */
  async list(): Promise<AppSnapshotMeta[]> {
    return await invoke("list_app_snapshots");
  },

  /** 保存当前状态为命名快照（同名覆盖） */
  async save(name: string): Promise<void> {
    return await invoke("save_app_snapshot", { name });
  },

  /** 应用快照（逐应用恢复供应商与接管状态，best-effort） */
  async apply(name: string): Promise<SnapshotApplyResult> {
    return await invoke("apply_app_snapshot", { name });
  },

  /** 删除快照 */
  async remove(name: string): Promise<void> {
    return await invoke("delete_app_snapshot", { name });
  },
};
