import { invoke } from "@tauri-apps/api/core";
import type { AppId } from "./types";

// ===== 连通性检查类型 =====
// 注意：检查分两级——先探测 base_url 可达性（不发真实大模型请求、不触碰故障
// 转移熔断器），可达后再用供应商凭据拉一次模型列表（GET /v1/models）验证
// API key 与 API 路径；列表获取失败降级为 degraded，不判 failed。

export type HealthStatus = "operational" | "degraded" | "failed";

export interface StreamCheckResult {
  status: HealthStatus;
  success: boolean;
  message: string;
  responseTimeMs?: number;
  httpStatus?: number;
  testedAt: number;
  retryCount: number;
}

// ===== 连通性检查 API =====

/**
 * 连通性检查（单个供应商）
 */
export async function streamCheckProvider(
  appType: AppId,
  providerId: string,
): Promise<StreamCheckResult> {
  return invoke("stream_check_provider", { appType, providerId });
}
