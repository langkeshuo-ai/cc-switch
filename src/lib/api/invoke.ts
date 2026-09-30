import { invoke } from "@tauri-apps/api/core";
import { extractErrorMessage } from "@/utils/errorUtils";

/**
 * 统一 invoke 出口：错误归一钩子 + 可选超时。
 *
 * 语义约定（与直接调用 `invoke` 一致）：
 * - 成功时原样返回后端数据，不做任何包装；
 * - 失败时**原样 rethrow**，不把错误替换成自定义 Error，避免破坏既有
 *   基于文案的匹配（i18n 映射、toast、`extractErrorMessage`）。
 *   钩子只做旁路观测（埋点/日志），不改变返回值。
 *
 * 渐进接入清单（`providers.ts` 为本轮样板，其余按需跟进）：
 * auth / config / connectivity-check / copilot / deeplink / env / failover /
 * globalProxy / mcp / model-fetch / pi / profiles / prompts / proxy /
 * sessions / settings / skills / snapshots / subscription / usage / vscode /
 * workspace / universal（providers.ts 内第二段）
 */
export interface InvokeOptions {
  /**
   * 超时毫秒数；不传则不限时（沿用 Tauri 默认行为）。
   * 只用于确实可能挂死的长命令，不要给高频读写加超时。
   *
   * 注意：这是**调用方**放弃等待，底层 IPC 无法被取消——后端命令仍会跑完，
   * 其副作用（写库/写配置）不因超时而回滚。只读命令用超时才是安全的。
   */
  timeoutMs?: number;
  /** 单次调用的错误旁路钩子，优先级高于全局钩子 */
  onError?: (info: InvokeErrorInfo) => void;
}

export interface InvokeErrorInfo {
  command: string;
  error: unknown;
  /** `extractErrorMessage(error)` 的结果，失败时为空串 */
  message: string;
}

export type InvokeErrorHook = (info: InvokeErrorInfo) => void;

let globalErrorHook: InvokeErrorHook | null = null;

/** 注册全局错误归一钩子（传 null 取消）；供日志/埋点统一接入 */
export const setInvokeErrorHook = (hook: InvokeErrorHook | null): void => {
  globalErrorHook = hook;
};

export const getInvokeErrorHook = (): InvokeErrorHook | null => globalErrorHook;

const raceTimeout = async <T>(
  pending: Promise<T>,
  timeoutMs: number,
  command: string,
): Promise<T> => {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      pending,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
          reject(new Error(`命令 ${command} 超时（${timeoutMs}ms）`));
        }, timeoutMs);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
};

export async function invokeCommand<T>(
  command: string,
  args?: Record<string, unknown>,
  options?: InvokeOptions,
): Promise<T> {
  const timeoutMs = options?.timeoutMs;
  try {
    const pending = invoke<T>(command, args);
    if (timeoutMs === undefined) {
      return await pending;
    }
    // 超时胜出后原始 promise 可能稍后才 reject；不挂一个 no-op catch 的话
    // 它会冒成 unhandled rejection，被 globalThis 的监听当成真错误上报。
    pending.catch(() => {});
    return await raceTimeout(pending, timeoutMs, command);
  } catch (error) {
    const info: InvokeErrorInfo = {
      command,
      error,
      message: extractErrorMessage(error),
    };
    try {
      globalErrorHook?.(info);
      options?.onError?.(info);
    } catch {
      // 钩子自身抛错不得影响原始错误的传播
    }
    throw error;
  }
}
