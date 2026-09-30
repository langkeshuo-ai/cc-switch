/**
 * 后端 `format_structured_error` 产出的结构化错误荷载
 * （`{"code","message"?,"context","suggestion"?}`）
 */
interface StructuredErrorPayload {
  code: string;
  message?: string;
  context?: Record<string, string>;
  suggestion?: string;
}

/**
 * 尝试把结构化错误荷载解析成人读文案；拿不到人读文案时返回 null。
 *
 * 渐进式错误码：后端仍是 `Result<T, String>`，只是荷载换成
 * `{"code","message"?,"context","suggestion"?}` 的 JSON。
 *
 * 只有携带 `message` 的荷载才会被展开——这些正是本轮新增的 provider 切换 /
 * 代理接管域。skill 域的历史荷载不含 `message`（见 `error.rs::format_skill_error`），
 * 对它们返回 null，`extractErrorMessage` 因而原样回退原文：既保持既有行为不变，
 * 也不会把 `code`/`suggestion` 这类机器码当文案丢给用户、更不会在日志里丢掉
 * `context`。需要按错误码做 i18n 的调用方请直接用 `parseSkillError`
 * （skills 域）或读取原始串自行映射。
 */
export const parseStructuredErrorMessage = (raw: string): string | null => {
  const trimmed = raw.trim();
  // 快速路径：绝大多数错误不是 JSON，避免无谓的 JSON.parse
  if (!trimmed.startsWith("{")) return null;
  try {
    const parsed: unknown = JSON.parse(trimmed);
    if (!parsed || typeof parsed !== "object") return null;
    const payload = parsed as Partial<StructuredErrorPayload>;
    if (typeof payload.code !== "string" || !payload.code) return null;
    if (typeof payload.message === "string" && payload.message.trim()) {
      return payload.message;
    }
    return null;
  } catch {
    return null;
  }
};

/**
 * 从各种错误对象中提取错误信息
 * @param error 错误对象
 * @returns 提取的错误信息字符串
 */
export const extractErrorMessage = (error: unknown): string => {
  if (!error) return "";
  if (typeof error === "string") {
    return parseStructuredErrorMessage(error) ?? error;
  }
  if (error instanceof Error && error.message.trim()) {
    return parseStructuredErrorMessage(error.message) ?? error.message;
  }

  if (typeof error === "object") {
    const errObject = error as Record<string, unknown>;

    const candidate = errObject.message ?? errObject.error ?? errObject.detail;
    if (typeof candidate === "string" && candidate.trim()) {
      return candidate;
    }

    const payload = errObject.payload;
    if (typeof payload === "string" && payload.trim()) {
      return payload;
    }
    if (payload && typeof payload === "object") {
      const payloadObj = payload as Record<string, unknown>;
      const payloadCandidate =
        payloadObj.message ?? payloadObj.error ?? payloadObj.detail;
      if (typeof payloadCandidate === "string" && payloadCandidate.trim()) {
        return payloadCandidate;
      }
    }
  }

  return "";
};

export const translatePiProviderMutationError = (
  message: string,
  t: (key: string, options?: Record<string, unknown>) => string,
): string => {
  if (!message) return "";

  if (
    message.includes("models.json changed") ||
    message.includes("changed outside CC Switch") ||
    message.includes("no longer present in models.json") ||
    message.includes("another value now owns the key")
  ) {
    return t("pi.provider.writeConflict");
  }

  if (message.includes("Pi provider") && message.includes("already exists")) {
    return t("pi.form.providerKeyDuplicate");
  }

  return "";
};

/**
 * 将已知的 MCP 相关后端错误（通常为中文硬编码）映射为 i18n 文案
 * 采用包含式匹配，尽量稳健地覆盖不同上下文的相似消息。
 * 若无法识别，返回空字符串以便调用方回退到原始 detail 或默认 i18n。
 */
export const translateMcpBackendError = (
  message: string,
  t: (key: string, opts?: any) => string,
): string => {
  if (!message) return "";
  const msg = String(message).trim();

  // 基础字段与结构校验相关
  if (msg.includes("MCP 服务器 ID 不能为空")) {
    return t("mcp.error.idRequired");
  }
  if (
    msg.includes("MCP 服务器定义必须为 JSON 对象") ||
    msg.includes("MCP 服务器条目必须为 JSON 对象") ||
    msg.includes("MCP 服务器条目缺少 server 字段") ||
    msg.includes("MCP 服务器 server 字段必须为 JSON 对象") ||
    msg.includes("MCP 服务器连接定义必须为 JSON 对象") ||
    msg.includes("MCP 服务器 '" /* 不是对象 */) ||
    msg.includes("不是对象") ||
    msg.includes("服务器配置必须是对象") ||
    msg.includes("MCP 服务器 name 必须为字符串") ||
    msg.includes("MCP 服务器 description 必须为字符串") ||
    msg.includes("MCP 服务器 homepage 必须为字符串") ||
    msg.includes("MCP 服务器 docs 必须为字符串") ||
    msg.includes("MCP 服务器 tags 必须为字符串数组") ||
    msg.includes("MCP 服务器 enabled 必须为布尔值")
  ) {
    return t("mcp.error.jsonInvalid");
  }
  if (msg.includes("MCP 服务器 type 必须是")) {
    return t("mcp.error.jsonInvalid");
  }

  // 必填字段
  if (
    msg.includes("stdio 类型的 MCP 服务器缺少 command 字段") ||
    msg.includes("必须包含 command 字段")
  ) {
    return t("mcp.error.commandRequired");
  }
  if (
    msg.includes("http 类型的 MCP 服务器缺少 url 字段") ||
    msg.includes("sse 类型的 MCP 服务器缺少 url 字段") ||
    msg.includes("必须包含 url 字段") ||
    msg === "URL 不能为空"
  ) {
    return t("mcp.wizard.urlRequired");
  }

  // 文件解析/序列化
  if (
    msg.includes("解析 ~/.claude.json 失败") ||
    msg.includes("解析 config.toml 失败") ||
    msg.includes("无法识别的 TOML 格式") ||
    msg.includes("TOML 内容不能为空")
  ) {
    return t("mcp.error.tomlInvalid");
  }
  if (msg.includes("序列化 config.toml 失败")) {
    return t("mcp.error.tomlInvalid");
  }

  return "";
};
