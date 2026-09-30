import { afterEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import {
  getInvokeErrorHook,
  invokeCommand,
  setInvokeErrorHook,
  type InvokeErrorInfo,
} from "./invoke";

afterEach(() => {
  invokeMock.mockReset();
  setInvokeErrorHook(null);
});

describe("invokeCommand", () => {
  it("passes command and args through and returns raw payload", async () => {
    invokeMock.mockResolvedValue({ id: "p1" });

    await expect(invokeCommand("get_provider", { id: "p1" })).resolves.toEqual({
      id: "p1",
    });
    expect(invokeMock).toHaveBeenCalledWith("get_provider", { id: "p1" });
  });

  it("rethrows the original rejection without wrapping it", async () => {
    invokeMock.mockRejectedValue("backend boom");

    await expect(invokeCommand("switch_provider", { id: "p1" })).rejects.toBe(
      "backend boom",
    );
  });

  it("reports normalized info to the global hook", async () => {
    const seen: InvokeErrorInfo[] = [];
    setInvokeErrorHook((info) => seen.push(info));
    invokeMock.mockRejectedValue(
      JSON.stringify({
        code: "PROVIDER_NOT_FOUND",
        message: "供应商 p1 不存在",
      }),
    );

    await expect(
      invokeCommand("switch_provider", { id: "p1" }),
    ).rejects.toBeTypeOf("string");
    expect(seen).toHaveLength(1);
    expect(seen[0]).toMatchObject({
      command: "switch_provider",
      message: "供应商 p1 不存在",
    });
    expect(getInvokeErrorHook()).not.toBeNull();
  });

  it("reports to the per-call hook as well", async () => {
    const seen: InvokeErrorInfo[] = [];
    invokeMock.mockRejectedValue(new Error("boom"));

    await expect(
      invokeCommand("cmd", undefined, { onError: (info) => seen.push(info) }),
    ).rejects.toBeInstanceOf(Error);
    expect(seen).toHaveLength(1);
  });

  it("does not let a throwing hook mask the original error", async () => {
    setInvokeErrorHook(() => {
      throw new Error("hook exploded");
    });
    invokeMock.mockRejectedValue("backend boom");

    await expect(invokeCommand("cmd")).rejects.toBe("backend boom");
  });

  it("rejects with a timeout error when timeoutMs elapses", async () => {
    invokeMock.mockImplementation(
      () => new Promise((resolve) => setTimeout(resolve, 50)),
    );

    await expect(
      invokeCommand("slow_cmd", undefined, { timeoutMs: 5 }),
    ).rejects.toThrow(/slow_cmd 超时/);
  });

  it("does not surface the late rejection of a timed-out command", async () => {
    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => unhandled.push(reason);
    process.on("unhandledRejection", onUnhandled);
    try {
      invokeMock.mockImplementation(
        () =>
          new Promise((_, reject) =>
            setTimeout(() => reject(new Error("late boom")), 60),
          ),
      );

      await expect(
        invokeCommand("slow_cmd", undefined, { timeoutMs: 5 }),
      ).rejects.toThrow(/slow_cmd 超时/);

      // 留足时间让迟到的 rejection 落地
      await new Promise((resolve) => setTimeout(resolve, 120));
      expect(unhandled).toHaveLength(0);
    } finally {
      process.off("unhandledRejection", onUnhandled);
    }
  });

  it("does not arm a timer when no timeout is requested", async () => {
    invokeMock.mockResolvedValue("ok");

    await expect(invokeCommand("fast_cmd")).resolves.toBe("ok");
    expect(invokeMock).toHaveBeenCalledWith("fast_cmd", undefined);
  });
});
