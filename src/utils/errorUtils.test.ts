import { describe, expect, it } from "vitest";
import { extractErrorMessage, parseStructuredErrorMessage } from "./errorUtils";

describe("parseStructuredErrorMessage", () => {
  it("returns the human message of a structured payload", () => {
    const payload = JSON.stringify({
      code: "PROVIDER_NOT_FOUND",
      message: "供应商 p1 不存在",
      context: { app: "claude", providerId: "p1" },
      suggestion: "refreshProviders",
    });
    expect(parseStructuredErrorMessage(payload)).toBe("供应商 p1 不存在");
  });

  it("returns null when the payload carries no human message", () => {
    // skill 域的历史荷载不含 message：必须原样回退，不把机器码当文案
    expect(
      parseStructuredErrorMessage(
        JSON.stringify({ code: "SKILL_NOT_FOUND", context: { name: "demo" } }),
      ),
    ).toBeNull();
    expect(
      parseStructuredErrorMessage(
        JSON.stringify({
          code: "TAKEOVER_FAILED",
          suggestion: "disableTakeover",
        }),
      ),
    ).toBeNull();
  });

  it("returns null for non-structured input", () => {
    expect(parseStructuredErrorMessage("供应商 p1 不存在")).toBeNull();
    expect(parseStructuredErrorMessage("")).toBeNull();
    expect(parseStructuredErrorMessage('{"detail":"boom"}')).toBeNull();
    expect(parseStructuredErrorMessage("{not json")).toBeNull();
  });
});

describe("extractErrorMessage", () => {
  it("unwraps structured payloads coming from invoke rejections", () => {
    const payload = JSON.stringify({
      code: "PROXY_TAKEOVER_HOT_SWITCH_FAILED",
      message: "热切换失败: proxy not running",
      context: { app: "codex" },
      suggestion: "disableTakeoverAndRetry",
    });
    expect(extractErrorMessage(payload)).toBe("热切换失败: proxy not running");
    expect(extractErrorMessage(new Error(payload))).toBe(
      "热切换失败: proxy not running",
    );
  });

  it("keeps legacy behaviour for plain messages", () => {
    expect(extractErrorMessage("普通错误")).toBe("普通错误");
    expect(extractErrorMessage(new Error("普通错误"))).toBe("普通错误");
    expect(extractErrorMessage({ message: "对象错误" })).toBe("对象错误");
    expect(extractErrorMessage(null)).toBe("");
  });

  it("leaves skill-domain payloads (no message key) byte-identical", () => {
    const payload = JSON.stringify({
      code: "SKILL_NOT_FOUND",
      context: { name: "demo" },
      suggestion: "retryLater",
    });
    expect(extractErrorMessage(payload)).toBe(payload);
    expect(extractErrorMessage(new Error(payload))).toBe(payload);
  });
});
